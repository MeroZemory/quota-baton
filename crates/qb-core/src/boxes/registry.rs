//! Ignored P4 common candidate; intended crate::boxes::registry child.
//! Caller owns the existing service/store lease and an owned blocking task.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::TryRngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use subtle::ConstantTimeEq;
use tokio::sync::watch;
use zeroize::Zeroizing;

#[path = "request_ownership.rs"]
mod request_ownership;
pub(crate) use request_ownership::BoxRequest;

const FILE_NAME: &str = "boxes.json";
const MAX_BOXES: usize = 256; // Tombstones count; never silently evict epochs.
const MAX_BYTES: usize = 256 * 1024;
const DOMAIN: &[u8] = b"quota-baton/box-key/v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RegistryError {
    #[error("invalid box name")]
    InvalidName,
    #[error("box registry is invalid")]
    InvalidData,
    #[error("box registry storage failed")]
    Storage,
    #[error("box key generation failed")]
    Entropy,
    #[error("box already exists")]
    AlreadyExists,
    #[error("box was not found")]
    NotFound,
    #[error("box registry capacity reached")]
    Capacity,
    #[error("invalid box credential")]
    Unauthorized,
    #[error("box registry synchronization failed")]
    Synchronization,
    #[error("box operation cancelled")]
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BoxName(String);

impl BoxName {
    pub fn parse(name: &str) -> Result<Self, RegistryError> {
        let alnum = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
        if name.is_empty()
            || name.len() > 63
            || !name.bytes().next().is_some_and(alnum)
            || !name
                .bytes()
                .all(|byte| alnum(byte) || matches!(byte, b'-' | b'_'))
        {
            return Err(RegistryError::InvalidName);
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Immutable identity; only the registry constructs epochs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoxScope {
    name: BoxName,
    epoch: u64,
}

impl BoxScope {
    pub fn name(&self) -> &BoxName {
        &self.name
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoxSummary {
    pub scope: BoxScope,
    pub revoked: bool,
}

/// No Clone/Serialize/raw-key Debug. Consuming this is the only key accessor.
pub(crate) struct IssuedBox {
    scope: BoxScope,
    key: Zeroizing<String>,
}

impl fmt::Debug for IssuedBox {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedBox")
            .field("scope", &self.scope)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl IssuedBox {
    pub fn into_parts(self) -> (BoxScope, Zeroizing<String>) {
        (self.scope, self.key)
    }
}

struct Grant {
    scope: BoxScope,
    revoked: AtomicBool,
    changed: watch::Sender<bool>,
    requests: request_ownership::RequestOwnership,
}

impl Grant {
    fn new(scope: BoxScope, revoked: bool) -> Self {
        let (changed, _) = watch::channel(revoked);
        Self {
            scope,
            revoked: AtomicBool::new(revoked),
            changed,
            requests: request_ownership::RequestOwnership::default(),
        }
    }

    fn revoke(&self) {
        // Never resurrect this object when a later epoch is enrolled.
        self.requests.close(&self.revoked);
        self.changed.send_replace(true);
    }
}

/// Immutable scoped evidence with live revocation, NOT account/launch authority.
#[derive(Clone)]
pub(crate) struct BoxAuthority(Arc<Grant>);

impl BoxAuthority {
    pub fn scope(&self) -> &BoxScope {
        &self.0.scope
    }
    pub fn is_revoked(&self) -> bool {
        self.0.revoked.load(Ordering::Acquire)
    }
    pub fn watch_revocation(&self) -> BoxRevocation {
        BoxRevocation(self.0.changed.subscribe())
    }

    /// Admits one exact-epoch lifetime, serialized with revocation. Clones of
    /// the returned permit retain that lifetime through detached descendants.
    pub fn admit(&self) -> Result<BoxRequest, RegistryError> {
        BoxRequest::admit(&self.0)
    }

    /// Waits for this epoch's admitted lifetimes, without a registry lock.
    /// Revoke first for a terminal drain; an open grant may admit work again.
    pub async fn wait_quiescent(&self) {
        self.0.requests.wait_quiescent().await;
    }
}

pub(crate) struct BoxRevocation(watch::Receiver<bool>);

impl BoxRevocation {
    /// Level-triggered: revocation before subscription/wait is still visible.
    /// Closure is fail-closed; this method does not close any network connection.
    pub async fn revoked(&mut self) {
        loop {
            if *self.0.borrow_and_update() {
                return;
            }
            if self.0.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    name: String,
    epoch: u64,
    digest: [u8; 32],
    revoked: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    boxes: Vec<Record>,
}

struct Entry {
    record: Record,
    grant: Arc<Grant>,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<BoxName, Entry>,
    dirty: bool,
}

struct Inner {
    path: PathBuf,
    state: Mutex<State>,
    persist: fn(&Path, &[u8]) -> io::Result<()>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Registry shutdown invalidates surviving grants; it is not persistence
        // of a permanent revocation and must not rewrite disk during Drop.
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner());
        for entry in state.entries.values() {
            entry.grant.revoke();
        }
    }
}

#[derive(Clone)]
pub(crate) struct BoxRegistry(Arc<Inner>);

impl BoxRegistry {
    /// Exactly one registry owner per home, cloned for callers. The external
    /// service lease serializes processes; this object never acquires a lease.
    pub fn open(home: &Path) -> Result<Self, RegistryError> {
        crate::platform::prepare_secret_home(home).map_err(|_| RegistryError::Storage)?;
        let path = home.join(FILE_NAME);
        let mut state = State::default();
        let bytes = match crate::platform::read_private_bytes(&path, MAX_BYTES) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => return Err(RegistryError::Storage),
        };
        if let Some(bytes) = bytes {
            if bytes.len() > MAX_BYTES {
                return Err(RegistryError::InvalidData);
            }
            let snapshot: Snapshot =
                serde_json::from_slice(&bytes).map_err(|_| RegistryError::InvalidData)?;
            if snapshot.version != 1 || snapshot.boxes.len() > MAX_BOXES {
                return Err(RegistryError::InvalidData);
            }
            for record in snapshot.boxes {
                let name = BoxName::parse(&record.name).map_err(|_| RegistryError::InvalidData)?;
                if record.epoch == 0 {
                    return Err(RegistryError::InvalidData);
                }
                let scope = BoxScope {
                    name: name.clone(),
                    epoch: record.epoch,
                };
                let grant = Arc::new(Grant::new(scope, record.revoked));
                if state
                    .entries
                    .insert(name, Entry { record, grant })
                    .is_some()
                {
                    return Err(RegistryError::InvalidData);
                }
            }
        }
        Ok(Self(Arc::new(Inner {
            path,
            state: Mutex::new(state),
            persist: write_snapshot,
        })))
    }

    pub fn list(&self) -> Result<Vec<BoxSummary>, RegistryError> {
        let state = self
            .0
            .state
            .lock()
            .map_err(|_| RegistryError::Synchronization)?;
        Ok(state
            .entries
            .values()
            .map(|entry| BoxSummary {
                scope: entry.grant.scope.clone(),
                revoked: entry.record.revoked,
            })
            .collect())
    }

    /// Host-admin launch selection, not client authentication. The caller must
    /// already be behind admin safety. Never queue request work behind fsync.
    pub(crate) fn try_authority(&self, name: &BoxName) -> Result<BoxAuthority, RegistryError> {
        let state = self
            .0
            .state
            .try_lock()
            .map_err(|_| RegistryError::Synchronization)?;
        if state.dirty {
            return Err(RegistryError::Storage);
        }
        let entry = state.entries.get(name).ok_or(RegistryError::NotFound)?;
        if entry.record.revoked || entry.grant.revoked.load(Ordering::Acquire) {
            return Err(RegistryError::Unauthorized);
        }
        Ok(BoxAuthority(entry.grant.clone()))
    }

    /// Host-side retirement lookup, NOT credential authentication. Returns the
    /// existing exact-epoch grant even after a failed durable revoke, so its
    /// admitted owners can still be drained. Never creates or reopens a grant.
    /// Retain the result across re-enrollment; an old epoch is not recoverable
    /// from the registry after its entry has been replaced by a new epoch.
    pub fn authority_for_retirement(
        &self,
        scope: &BoxScope,
    ) -> Result<BoxAuthority, RegistryError> {
        let state = self
            .0
            .state
            .try_lock()
            .map_err(|_| RegistryError::Synchronization)?;
        let entry = state
            .entries
            .get(scope.name())
            .filter(|entry| entry.grant.scope == *scope)
            .ok_or(RegistryError::NotFound)?;
        Ok(BoxAuthority(entry.grant.clone()))
    }

    /// New name starts at epoch 1; re-enrollment requires a tombstone and uses
    /// the next epoch. Raw key never enters registry state or serialized bytes.
    #[cfg(test)]
    pub fn add(&self, name: BoxName) -> Result<IssuedBox, RegistryError> {
        self.add_admitted(name, None)
    }

    pub fn add_for_operation(
        &self,
        name: BoxName,
        operation: &crate::issue_operation::IssueOperation,
    ) -> Result<IssuedBox, RegistryError> {
        self.add_admitted(name, Some(operation))
    }

    fn add_admitted(
        &self,
        name: BoxName,
        operation: Option<&crate::issue_operation::IssueOperation>,
    ) -> Result<IssuedBox, RegistryError> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| RegistryError::Synchronization)?;
        if let Some(operation) = operation {
            operation.check().map_err(|_| RegistryError::Cancelled)?;
        }
        // Retry a failed revocation first; no enrollment may overtake it.
        if state.dirty {
            return Err(RegistryError::Storage);
        }
        let epoch = match state.entries.get(&name) {
            Some(entry) if !entry.record.revoked => return Err(RegistryError::AlreadyExists),
            Some(entry) => entry
                .record
                .epoch
                .checked_add(1)
                .ok_or(RegistryError::Capacity)?,
            None if state.entries.len() >= MAX_BOXES => return Err(RegistryError::Capacity),
            None => 1,
        };
        let mut entropy = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng
            .try_fill_bytes(&mut *entropy)
            .map_err(|_| RegistryError::Entropy)?;
        let key = Zeroizing::new(URL_SAFE_NO_PAD.encode(entropy.as_slice()));
        let scope = BoxScope {
            name: name.clone(),
            epoch,
        };
        let digest = key_digest(&scope, &key);
        if let Some(operation) = operation {
            operation.admit().map_err(|_| RegistryError::Cancelled)?;
        }
        state.entries.insert(
            name.clone(),
            Entry {
                record: Record {
                    name: name.as_str().to_owned(),
                    epoch,
                    digest,
                    revoked: false,
                },
                grant: Arc::new(Grant::new(scope.clone(), false)),
            },
        );
        if self.persist(&state).is_err() {
            // Rename+fsync can fail after disk publication. The undisclosed
            // fresh key is dropped/wiped; this live epoch stays disabled.
            let entry = state.entries.get_mut(&name).expect("just inserted");
            entry.record.revoked = true;
            entry.grant.revoke();
            state.dirty = true;
            return Err(RegistryError::Storage);
        }
        Ok(IssuedBox { scope, key })
    }

    /// Returns success ONLY after a durable tombstone write. Failure leaves
    /// grants/watchers revoked in memory; repeating revoke retries persistence.
    pub fn revoke(&self, name: &BoxName) -> Result<BoxScope, RegistryError> {
        self.revoke_admitted(name, None)
    }

    pub fn revoke_for_operation(
        &self,
        name: &BoxName,
        operation: &crate::issue_operation::IssueOperation,
    ) -> Result<BoxScope, RegistryError> {
        self.revoke_admitted(name, Some(operation))
    }

    fn revoke_admitted(
        &self,
        name: &BoxName,
        operation: Option<&crate::issue_operation::IssueOperation>,
    ) -> Result<BoxScope, RegistryError> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| RegistryError::Synchronization)?;
        let entry = state.entries.get_mut(name).ok_or(RegistryError::NotFound)?;
        if let Some(operation) = operation {
            operation.admit().map_err(|_| RegistryError::Cancelled)?;
        }
        entry.grant.revoke();
        entry.record.revoked = true;
        let scope = entry.grant.scope.clone();
        state.dirty = true;
        self.persist(&state)?;
        state.dirty = false;
        Ok(scope)
    }

    /// `scope` comes from the selected trusted socket, never a client header.
    #[cfg(test)]
    pub fn authenticate(&self, scope: &BoxScope, key: &str) -> Result<BoxAuthority, RegistryError> {
        self.authenticate_with_lock(scope, key, false)
    }

    /// Request ingress never blocks behind registry fsync. Contention is a
    /// fixed temporary refusal, not permission or a queued background job.
    pub fn try_authenticate(
        &self,
        scope: &BoxScope,
        key: &str,
    ) -> Result<BoxAuthority, RegistryError> {
        self.authenticate_with_lock(scope, key, true)
    }

    /// A key identifies its box independently of any optional listener binding.
    /// Bound listeners additionally require the resulting exact scope to match.
    pub fn try_identify(&self, key: &str) -> Result<BoxAuthority, RegistryError> {
        if !valid_key(key) {
            return Err(RegistryError::Unauthorized);
        }
        let state = self
            .0
            .state
            .try_lock()
            .map_err(|_| RegistryError::Synchronization)?;
        let mut found = None;
        let mut ambiguous = false;
        // The registry is capped at MAX_BOXES. Compare all stored digests;
        // never accept a first match before excluding ambiguous identities.
        for entry in state.entries.values() {
            let candidate = key_digest(&entry.grant.scope, key);
            let matches = bool::from(candidate.ct_eq(&entry.record.digest));
            if matches && !entry.record.revoked && !entry.grant.revoked.load(Ordering::Acquire) {
                ambiguous |= found.is_some();
                found = Some(BoxAuthority(entry.grant.clone()));
            }
        }
        if ambiguous {
            return Err(RegistryError::Unauthorized);
        }
        found.ok_or(RegistryError::Unauthorized)
    }

    fn authenticate_with_lock(
        &self,
        scope: &BoxScope,
        key: &str,
        nonblocking: bool,
    ) -> Result<BoxAuthority, RegistryError> {
        // Fixed-size canonical generated key; no allocation from hostile input.
        if !valid_key(key) {
            return Err(RegistryError::Unauthorized);
        }
        let state = if nonblocking {
            self.0
                .state
                .try_lock()
                .map_err(|_| RegistryError::Synchronization)?
        } else {
            self.0
                .state
                .lock()
                .map_err(|_| RegistryError::Synchronization)?
        };
        let entry = state
            .entries
            .get(&scope.name)
            .ok_or(RegistryError::Unauthorized)?;
        let candidate = key_digest(scope, key);
        let matches = bool::from(candidate.ct_eq(&entry.record.digest));
        if !matches
            || entry.record.revoked
            || entry.grant.scope != *scope
            || entry.grant.revoked.load(Ordering::Acquire)
        {
            return Err(RegistryError::Unauthorized);
        }
        Ok(BoxAuthority(entry.grant.clone()))
    }

    /// Host-admin publication gate, NOT authentication. Scope must already come
    /// from the trusted current registry selection. Callback must be short,
    /// synchronous, non-reentrant and free of I/O/awaits. The only allowed nested
    /// lock is the existing short operation.can_reply() state gate, in that order.
    pub fn try_with_active_scope<R>(
        &self,
        scope: &BoxScope,
        publish: impl FnOnce() -> R,
    ) -> Result<R, RegistryError> {
        let state = self
            .0
            .state
            .try_lock()
            .map_err(|_| RegistryError::Synchronization)?;
        if state.dirty {
            return Err(RegistryError::Storage);
        }
        let entry = state
            .entries
            .get(&scope.name)
            .ok_or(RegistryError::Unauthorized)?;
        if entry.record.revoked
            || entry.grant.scope != *scope
            || entry.grant.revoked.load(Ordering::Acquire)
        {
            return Err(RegistryError::Unauthorized);
        }
        let result = publish();
        drop(state);
        Ok(result)
    }

    fn persist(&self, state: &State) -> Result<(), RegistryError> {
        let snapshot = Snapshot {
            version: 1,
            boxes: state
                .entries
                .values()
                .map(|entry| entry.record.clone())
                .collect(),
        };
        let bytes = serde_json::to_vec(&snapshot).map_err(|_| RegistryError::InvalidData)?;
        if bytes.len() > MAX_BYTES {
            return Err(RegistryError::Capacity);
        }
        (self.0.persist)(&self.0.path, &bytes).map_err(|_| RegistryError::Storage)
    }
}

fn valid_key(key: &str) -> bool {
    key.len() == 43
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn key_digest(scope: &BoxScope, key: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(DOMAIN);
    hash.update((scope.name.as_str().len() as u64).to_be_bytes());
    hash.update(scope.name.as_str().as_bytes());
    hash.update(scope.epoch.to_be_bytes());
    hash.update(key.as_bytes());
    hash.finalize().into()
}

fn write_snapshot(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // Actual existing native facade: Unix private leaf+rename+file/dir fsync;
    // Windows restricted temporary ACL+flush+write-through MoveFileExW.
    crate::platform::atomic_write_binding(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_mutations_waiting_for_registry_gate_do_not_change_durable_identity() {
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let (scope, key) = registry
            .add(BoxName::parse("retained").unwrap())
            .unwrap()
            .into_parts();
        let before = std::fs::read(home.path().join(FILE_NAME)).unwrap();
        for revoke in [false, true] {
            let operation = crate::issue_operation::IssueOperation::new(
                tokio::time::Instant::now() + std::time::Duration::from_secs(30),
            );
            let caller = operation.caller_guard();
            let held = registry.0.state.lock().unwrap();
            let target = registry.clone();
            let name = scope.name().clone();
            let (send, entered) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                send.send(()).unwrap();
                if revoke {
                    target.revoke_for_operation(&name, &operation).map(|_| ())
                } else {
                    target
                        .add_for_operation(BoxName::parse("cancelled-new").unwrap(), &operation)
                        .map(|_| ())
                }
            });
            entered.recv().unwrap();
            drop(caller);
            drop(held);
            assert_eq!(worker.join().unwrap(), Err(RegistryError::Cancelled));
            assert_eq!(std::fs::read(home.path().join(FILE_NAME)).unwrap(), before);
            assert!(registry.try_authenticate(&scope, &key).is_ok());
        }
    }

    #[test]
    fn key_identity_is_independent_of_listener_binding_and_old_epochs() {
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let (scope_a, key_a) = registry
            .add(BoxName::parse("sample-a").unwrap())
            .unwrap()
            .into_parts();
        let (scope_b, key_b) = registry
            .add(BoxName::parse("sample-b").unwrap())
            .unwrap()
            .into_parts();
        assert_eq!(registry.try_identify(&key_a).unwrap().scope(), &scope_a);
        assert_eq!(registry.try_identify(&key_b).unwrap().scope(), &scope_b);
        assert!(matches!(
            registry.try_authenticate(&scope_b, &key_a),
            Err(RegistryError::Unauthorized)
        ));
        registry.revoke(scope_a.name()).unwrap();
        assert!(matches!(
            registry.try_identify(&key_a),
            Err(RegistryError::Unauthorized)
        ));
        assert_eq!(registry.try_identify(&key_b).unwrap().scope(), &scope_b);
        let (next_a, next_key) = registry.add(scope_a.name().clone()).unwrap().into_parts();
        assert_ne!(next_a, scope_a);
        assert_eq!(registry.try_identify(&next_key).unwrap().scope(), &next_a);
        assert!(matches!(
            registry.try_authenticate(&scope_a, &next_key),
            Err(RegistryError::Unauthorized)
        ));
        let held = registry.0.state.lock().unwrap();
        assert!(matches!(
            registry.try_identify(&key_b),
            Err(RegistryError::Synchronization)
        ));
        drop(held);
    }

    #[test]
    fn request_auth_uses_same_exact_key_and_refuses_busy_registry_without_waiting() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/tests/box-auth-try");
        std::fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let (scope, key) = registry
            .add(BoxName::parse("sample-box").unwrap())
            .unwrap()
            .into_parts();
        assert_eq!(
            registry.try_authenticate(&scope, &key).unwrap().scope(),
            &scope
        );
        let bad = "A".repeat(43);
        assert!(matches!(
            registry.try_authenticate(&scope, &bad),
            Err(RegistryError::Unauthorized)
        ));
        let held = registry.0.state.lock().unwrap();
        let worker_registry = registry.clone();
        let worker_scope = scope.clone();
        let worker_key = Zeroizing::new(key.to_string());
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = worker_registry.try_authenticate(&worker_scope, &worker_key);
            send.send(result.err()).unwrap();
        });
        let result = receive.recv_timeout(std::time::Duration::from_secs(2));
        drop(held); // Also lets a broken blocking implementation terminate cleanly.
        worker.join().unwrap();
        assert_eq!(result.unwrap(), Some(RegistryError::Synchronization));
        registry.revoke(scope.name()).unwrap();
        assert!(matches!(
            registry.try_authenticate(&scope, &key),
            Err(RegistryError::Unauthorized)
        ));
    }

    async fn pending_waiter(mut watcher: BoxRevocation) -> tokio::task::JoinHandle<()> {
        use std::future::Future as _;
        let (ready, started) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let mut ready = Some(ready);
            let wait = watcher.revoked();
            tokio::pin!(wait);
            std::future::poll_fn(|cx| {
                let result = wait.as_mut().poll(cx);
                if let Some(ready) = ready.take() {
                    assert!(
                        result.is_pending(),
                        "waiter must register before revocation"
                    );
                    ready.send(()).unwrap();
                }
                result
            })
            .await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), started)
            .await
            .unwrap()
            .unwrap();
        waiter
    }

    async fn observe_revoked(watcher: &mut BoxRevocation) {
        tokio::time::timeout(std::time::Duration::from_secs(1), watcher.revoked())
            .await
            .unwrap();
    }

    fn home() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/tests/box-registry");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(base).unwrap()
    }

    #[tokio::test]
    async fn revoke_wins_between_authentication_and_request_admission() {
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let (scope, key) = registry
            .add(BoxName::parse("request-race").unwrap())
            .unwrap()
            .into_parts();
        let authority = registry.try_authenticate(&scope, &key).unwrap();
        let mut watcher = authority.watch_revocation();
        let (ready, started) = tokio::sync::oneshot::channel();
        let (proceed, revoked) = tokio::sync::oneshot::channel();
        // Deterministically pause the actual authenticated contender before
        // admission, then let actual registry revocation win the shared gate.
        let contender = async move {
            ready.send(()).unwrap();
            revoked.await.unwrap();
            authority.admit()
        };
        let revoker = async {
            started.await.unwrap();
            registry.revoke(scope.name()).unwrap();
            proceed.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(contender, revoker);
        assert!(matches!(result, Err(RegistryError::Unauthorized)));
        observe_revoked(&mut watcher).await;
    }

    #[tokio::test]
    async fn admitted_request_clones_hold_all_quiescent_waiters_until_last_drop() {
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let (scope, key) = registry
            .add(BoxName::parse("request-clones").unwrap())
            .unwrap()
            .into_parts();
        let authority = registry.try_authenticate(&scope, &key).unwrap();
        let request = authority.admit().unwrap();
        let descendant = request.clone();
        assert_eq!(request.scope(), &scope);
        registry.revoke(scope.name()).unwrap();
        assert!(request.is_revoked());
        assert!(matches!(
            authority.admit(),
            Err(RegistryError::Unauthorized)
        ));
        let mut first = Box::pin(authority.wait_quiescent());
        let mut second = Box::pin(authority.wait_quiescent());
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        drop(request);
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        // Already-admitted cleanup can retain another descendant after revoke.
        let final_descendant = descendant.clone();
        drop(descendant);
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        drop(final_descendant);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::join!(first, second);
        })
        .await
        .unwrap();
        // Registration after the final notification must also complete.
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            authority.wait_quiescent(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reenrolled_epoch_has_independent_request_admission_and_drain() {
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let name = BoxName::parse("request-epochs").unwrap();
        let (old_scope, old_key) = registry.add(name.clone()).unwrap().into_parts();
        let old = registry.try_authenticate(&old_scope, &old_key).unwrap();
        let old_request = old.admit().unwrap();
        registry.revoke(&name).unwrap();
        let (new_scope, new_key) = registry.add(name).unwrap().into_parts();
        let new = registry.try_authenticate(&new_scope, &new_key).unwrap();
        let new_request = new.admit().unwrap();
        assert_ne!(old_scope, new_scope);
        assert_eq!(new_request.scope(), &new_scope);
        assert!(old_request.is_revoked());
        assert!(!new_request.is_revoked());
        assert!(matches!(old.admit(), Err(RegistryError::Unauthorized)));
        let mut old_wait = Box::pin(old.wait_quiescent());
        let mut new_wait = Box::pin(new.wait_quiescent());
        assert!(futures_util::poll!(old_wait.as_mut()).is_pending());
        assert!(futures_util::poll!(new_wait.as_mut()).is_pending());
        drop(old_request);
        tokio::time::timeout(std::time::Duration::from_secs(1), old_wait)
            .await
            .unwrap();
        assert!(futures_util::poll!(new_wait.as_mut()).is_pending());
        assert!(!new_request.is_revoked());
        drop(new_request);
        tokio::time::timeout(std::time::Duration::from_secs(1), new_wait)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_durable_revoke_closes_admission_and_retains_existing_request() {
        let home = home();
        let mut registry = BoxRegistry::open(home.path()).unwrap();
        let (scope, key) = registry
            .add(BoxName::parse("request-failed-revoke").unwrap())
            .unwrap()
            .into_parts();
        let authority = registry.try_authenticate(&scope, &key).unwrap();
        let request = authority.admit().unwrap();
        Arc::get_mut(&mut registry.0).unwrap().persist = refuse_write;
        assert_eq!(registry.revoke(scope.name()), Err(RegistryError::Storage));
        assert!(request.is_revoked());
        let retirement = registry.authority_for_retirement(&scope).unwrap();
        assert!(Arc::ptr_eq(&retirement.0, &authority.0));
        assert!(matches!(
            retirement.admit(),
            Err(RegistryError::Unauthorized)
        ));
        assert!(matches!(
            authority.admit(),
            Err(RegistryError::Unauthorized)
        ));
        let mut wait = Box::pin(authority.wait_quiescent());
        assert!(futures_util::poll!(wait.as_mut()).is_pending());
        drop(request);
        tokio::time::timeout(std::time::Duration::from_secs(1), wait)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn retirement_lookup_retains_revoked_owners_and_refuses_cross_epoch() {
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let name = BoxName::parse("retirement-lookup").unwrap();
        let (scope, key) = registry.add(name.clone()).unwrap().into_parts();
        let authenticated = registry.try_authenticate(&scope, &key).unwrap();
        let request = authenticated.admit().unwrap();
        let before = registry.authority_for_retirement(&scope).unwrap();
        assert!(Arc::ptr_eq(&before.0, &authenticated.0));
        assert!(!before.is_revoked());
        registry.revoke(&name).unwrap();
        let retirement = registry.authority_for_retirement(&scope).unwrap();
        assert!(Arc::ptr_eq(&retirement.0, &authenticated.0));
        assert!(retirement.is_revoked());
        assert!(matches!(
            retirement.admit(),
            Err(RegistryError::Unauthorized)
        ));
        let mut wait = Box::pin(retirement.wait_quiescent());
        assert!(futures_util::poll!(wait.as_mut()).is_pending());
        let (replacement, _) = registry.add(name).unwrap().into_parts();
        assert_ne!(scope, replacement);
        assert!(matches!(
            registry.authority_for_retirement(&scope),
            Err(RegistryError::NotFound)
        ));
        let current = registry.authority_for_retirement(&replacement).unwrap();
        assert_eq!(current.scope(), &replacement);
        assert!(!current.is_revoked());
        let absent = BoxScope {
            name: BoxName::parse("retirement-absent").unwrap(),
            epoch: 1,
        };
        assert!(matches!(
            registry.authority_for_retirement(&absent),
            Err(RegistryError::NotFound)
        ));
        // A retained old authority still waits for the OLD owners after the
        // registry entry changes; the new epoch cannot satisfy that wait.
        assert!(futures_util::poll!(wait.as_mut()).is_pending());
        drop(request);
        tokio::time::timeout(std::time::Duration::from_secs(1), wait)
            .await
            .unwrap();
        assert!(retirement.is_revoked());
    }

    #[test]
    fn names_are_bounded_ascii_components() {
        for good in ["a", "0", "box_a-2", &"z".repeat(63)] {
            assert!(BoxName::parse(good).is_ok());
        }
        for bad in [
            "",
            "A",
            "_box",
            "-box",
            ".",
            "..",
            "a.b",
            "a/b",
            "a\\b",
            "a b",
            "한글",
            "a\0",
            &"z".repeat(64),
        ] {
            assert_eq!(BoxName::parse(bad), Err(RegistryError::InvalidName));
        }
    }

    #[tokio::test]
    async fn key_socket_epoch_binding_revocation_and_reopen_use_real_storage() {
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let a = BoxName::parse("box-a").unwrap();
        let b = BoxName::parse("box-b").unwrap();
        let (scope_a, key_a) = registry.add(a.clone()).unwrap().into_parts();
        let (scope_b, key_b) = registry.add(b.clone()).unwrap().into_parts();
        assert_eq!(URL_SAFE_NO_PAD.decode(key_a.as_bytes()).unwrap().len(), 32);
        assert_ne!(*key_a, *key_b);
        let authority = registry.authenticate(&scope_a, &key_a).unwrap();
        let mut watching = authority.watch_revocation();
        assert!(registry.authenticate(&scope_b, &key_a).is_err());
        assert!(registry.authenticate(&scope_a, &key_b).is_err());
        registry.revoke(&a).unwrap();
        assert!(authority.is_revoked());
        observe_revoked(&mut watching).await;
        observe_revoked(&mut authority.watch_revocation()).await;
        assert!(registry.authenticate(&scope_a, &key_a).is_err());
        assert!(registry.authenticate(&scope_b, &key_b).is_ok());
        let bytes =
            crate::platform::read_private_bytes(&home.path().join(FILE_NAME), MAX_BYTES).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains(key_a.as_str()) && !text.contains(key_b.as_str()));
        let snapshot: Snapshot = serde_json::from_slice(&bytes).unwrap();
        assert!(
            snapshot
                .boxes
                .iter()
                .find(|record| record.name == "box-a")
                .unwrap()
                .revoked
        );
        drop(registry);
        let reopened = BoxRegistry::open(home.path()).unwrap();
        assert!(reopened.authenticate(&scope_a, &key_a).is_err());
        assert!(reopened.authenticate(&scope_b, &key_b).is_ok());
        let (new_scope, new_key) = reopened.add(a).unwrap().into_parts();
        assert_eq!(new_scope.epoch(), scope_a.epoch() + 1);
        assert!(reopened.authenticate(&scope_a, &new_key).is_err());
        assert!(reopened.authenticate(&new_scope, &key_a).is_err());
        assert!(reopened.authenticate(&new_scope, &new_key).is_ok());
        assert!(authority.is_revoked());
        assert_eq!(reopened.list().unwrap().len(), 2);
    }

    fn refuse_write(_: &Path, _: &[u8]) -> io::Result<()> {
        Err(io::ErrorKind::PermissionDenied.into())
    }
    fn write_then_fail(path: &Path, bytes: &[u8]) -> io::Result<()> {
        write_snapshot(path, bytes)?;
        Err(io::ErrorKind::Other.into())
    }

    #[tokio::test]
    async fn failed_durable_revoke_stays_live_revoked_and_retry_persists_tombstone() {
        for persist in [
            refuse_write as fn(&Path, &[u8]) -> io::Result<()>,
            write_then_fail,
        ] {
            let home = home();
            let mut registry = BoxRegistry::open(home.path()).unwrap();
            let name = BoxName::parse("box-a").unwrap();
            let (scope, key) = registry.add(name.clone()).unwrap().into_parts();
            let authority = registry.authenticate(&scope, &key).unwrap();
            let mut watcher = authority.watch_revocation();
            Arc::get_mut(&mut registry.0).unwrap().persist = persist;
            assert_eq!(registry.revoke(&name), Err(RegistryError::Storage));
            assert!(authority.is_revoked());
            observe_revoked(&mut watcher).await;
            assert!(registry.authenticate(&scope, &key).is_err());
            assert!(matches!(
                registry.add(BoxName::parse("box-b").unwrap()),
                Err(RegistryError::Storage)
            ));
            Arc::get_mut(&mut registry.0).unwrap().persist = write_snapshot;
            registry.revoke(&name).unwrap();
            drop(registry);
            let reopened = BoxRegistry::open(home.path()).unwrap();
            assert!(reopened.authenticate(&scope, &key).is_err());
            assert!(reopened.list().unwrap()[0].revoked);
        }
    }

    #[test]
    fn failed_add_does_not_return_key_before_or_after_publication() {
        for (persist, published) in [
            (refuse_write as fn(&Path, &[u8]) -> io::Result<()>, false),
            (write_then_fail, true),
        ] {
            let home = home();
            let mut registry = BoxRegistry::open(home.path()).unwrap();
            let name = BoxName::parse("box-a").unwrap();
            Arc::get_mut(&mut registry.0).unwrap().persist = persist;
            assert!(matches!(
                registry.add(name.clone()),
                Err(RegistryError::Storage)
            ));
            assert!(registry.list().unwrap()[0].revoked);
            assert!(registry.0.state.lock().unwrap().dirty);
            assert!(matches!(
                registry.add(name.clone()),
                Err(RegistryError::Storage)
            ));
            let disk = crate::platform::read_private_bytes(&home.path().join(FILE_NAME), MAX_BYTES);
            if published {
                let snapshot: Snapshot = serde_json::from_slice(&disk.unwrap()).unwrap();
                assert_eq!(snapshot.boxes.len(), 1);
                assert_eq!(snapshot.boxes[0].epoch, 1);
                assert!(!snapshot.boxes[0].revoked);
            } else {
                assert_eq!(disk.unwrap_err().kind(), io::ErrorKind::NotFound);
            }
            Arc::get_mut(&mut registry.0).unwrap().persist = write_snapshot;
            registry.revoke(&name).unwrap();
            drop(registry);
            assert!(BoxRegistry::open(home.path()).unwrap().list().unwrap()[0].revoked);
        }
    }

    #[test]
    fn failed_reenrollment_preserves_revocation_and_never_reuses_failed_epoch() {
        for (persist, published) in [
            (refuse_write as fn(&Path, &[u8]) -> io::Result<()>, false),
            (write_then_fail, true),
        ] {
            let home = home();
            let mut registry = BoxRegistry::open(home.path()).unwrap();
            let name = BoxName::parse("box-a").unwrap();
            let (old_scope, old_key) = registry.add(name.clone()).unwrap().into_parts();
            let authority = registry.authenticate(&old_scope, &old_key).unwrap();
            registry.revoke(&name).unwrap();
            Arc::get_mut(&mut registry.0).unwrap().persist = persist;
            assert!(matches!(
                registry.add(name.clone()),
                Err(RegistryError::Storage)
            ));
            let failed = registry.list().unwrap().remove(0);
            assert_eq!(failed.scope.epoch(), old_scope.epoch() + 1);
            assert!(failed.revoked && authority.is_revoked());
            assert!(registry.authenticate(&old_scope, &old_key).is_err());
            assert!(registry.authenticate(&failed.scope, &old_key).is_err());
            assert!(matches!(
                registry.add(name.clone()),
                Err(RegistryError::Storage)
            ));
            let disk = crate::platform::read_private_bytes(&home.path().join(FILE_NAME), MAX_BYTES)
                .unwrap();
            let snapshot: Snapshot = serde_json::from_slice(&disk).unwrap();
            assert_eq!(snapshot.boxes.len(), 1);
            assert_eq!(snapshot.boxes[0].epoch, if published { 2 } else { 1 });
            assert_eq!(snapshot.boxes[0].revoked, !published);
            Arc::get_mut(&mut registry.0).unwrap().persist = write_snapshot;
            registry.revoke(&name).unwrap();
            drop(registry);
            let reopened = BoxRegistry::open(home.path()).unwrap();
            let tombstone = reopened.list().unwrap().remove(0);
            assert!(tombstone.revoked);
            assert_eq!(tombstone.scope, failed.scope);
            let (scope, key) = reopened.add(name).unwrap().into_parts();
            assert_eq!(scope.epoch(), failed.scope.epoch() + 1);
            assert!(reopened.authenticate(&failed.scope, &key).is_err());
            assert!(reopened.authenticate(&scope, &old_key).is_err());
            assert!(reopened.authenticate(&scope, &key).is_ok());
            assert!(authority.is_revoked());
        }
    }

    #[tokio::test]
    async fn pending_revocation_waiter_wakes_on_revoke() {
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let name = BoxName::parse("box-a").unwrap();
        let (scope, key) = registry.add(name.clone()).unwrap().into_parts();
        let authority = registry.authenticate(&scope, &key).unwrap();
        let waiter = pending_waiter(authority.watch_revocation()).await;
        registry.revoke(&name).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(authority.is_revoked());
    }

    #[tokio::test]
    async fn dropping_last_registry_revokes_existing_authority_and_wakes_pending_waiter() {
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let (scope, key) = registry
            .add(BoxName::parse("box-a").unwrap())
            .unwrap()
            .into_parts();
        let authority = registry.authenticate(&scope, &key).unwrap();
        let clone = registry.clone();
        let waiter = pending_waiter(authority.watch_revocation()).await;
        drop(registry);
        assert!(!authority.is_revoked());
        assert!(!waiter.is_finished());
        drop(clone);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(authority.is_revoked());
    }

    #[test]
    fn publication_rejects_absent_mismatched_revoked_and_stale_real_scopes() {
        use std::cell::Cell;
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let a = BoxName::parse("box-a").unwrap();
        let b = BoxName::parse("box-b").unwrap();
        let (scope_a, _) = registry.add(a.clone()).unwrap().into_parts();
        registry.add(b.clone()).unwrap();
        assert_eq!(registry.try_with_active_scope(&scope_a, || 7), Ok(7));

        // Mint real scopes in another isolated registry; do not fabricate fields.
        let other_home = self::home();
        let other = BoxRegistry::open(other_home.path()).unwrap();
        let (absent, _) = other
            .add(BoxName::parse("box-c").unwrap())
            .unwrap()
            .into_parts();
        other.add(b.clone()).unwrap();
        other.revoke(&b).unwrap();
        let (mismatched, _) = other.add(b).unwrap().into_parts();
        let called = Cell::new(false);
        for scope in [&absent, &mismatched] {
            assert_eq!(
                registry.try_with_active_scope(scope, || called.set(true)),
                Err(RegistryError::Unauthorized)
            );
        }
        registry.revoke(&a).unwrap();
        assert_eq!(
            registry.try_with_active_scope(&scope_a, || called.set(true)),
            Err(RegistryError::Unauthorized)
        );
        let (current, _) = registry.add(a).unwrap().into_parts();
        assert_eq!(
            registry.try_with_active_scope(&scope_a, || called.set(true)),
            Err(RegistryError::Unauthorized)
        );
        assert!(!called.get());
        assert_eq!(registry.try_with_active_scope(&current, || 9), Ok(9));
    }

    #[test]
    fn publication_refuses_busy_gate_without_waiting_or_calling_callback() {
        use std::sync::mpsc;
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let (scope, _) = registry
            .add(BoxName::parse("box-a").unwrap())
            .unwrap()
            .into_parts();
        let called = Arc::new(AtomicBool::new(false));
        let contender = registry.clone();
        let contender_called = called.clone();
        let (send, receive) = mpsc::channel();
        let guard = registry.0.state.lock().unwrap();
        let worker = std::thread::spawn(move || {
            let result = contender.try_with_active_scope(&scope, || {
                contender_called.store(true, Ordering::Release);
            });
            send.send(result).unwrap();
        });
        let result = receive.recv_timeout(std::time::Duration::from_secs(1));
        // Release before asserting/joining, so an accidentally blocking
        // implementation fails the timeout without stranding the worker.
        drop(guard);
        worker.join().unwrap();
        assert_eq!(result.unwrap(), Err(RegistryError::Synchronization));
        assert!(!called.load(Ordering::Acquire));
    }

    #[test]
    fn publication_refuses_dirty_or_poisoned_gate_without_callback() {
        use std::cell::Cell;
        let home = home();
        let mut registry = BoxRegistry::open(home.path()).unwrap();
        let a = BoxName::parse("box-a").unwrap();
        registry.add(a.clone()).unwrap();
        let (scope_b, _) = registry
            .add(BoxName::parse("box-b").unwrap())
            .unwrap()
            .into_parts();
        Arc::get_mut(&mut registry.0).unwrap().persist = refuse_write;
        assert_eq!(registry.revoke(&a), Err(RegistryError::Storage));
        let called = Cell::new(false);
        assert_eq!(
            registry.try_with_active_scope(&scope_b, || called.set(true)),
            Err(RegistryError::Storage)
        );
        Arc::get_mut(&mut registry.0).unwrap().persist = write_snapshot;
        registry.revoke(&a).unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = registry.try_with_active_scope::<()>(&scope_b, || {
                panic!("synthetic publication panic");
            });
        }));
        assert!(panic.is_err());
        assert_eq!(
            registry.try_with_active_scope(&scope_b, || called.set(true)),
            Err(RegistryError::Synchronization)
        );
        assert!(!called.get());
    }

    #[test]
    fn competing_revoke_waits_until_publication_callback_returns() {
        use std::sync::mpsc;
        let home = home();
        let registry = BoxRegistry::open(home.path()).unwrap();
        let name = BoxName::parse("box-a").unwrap();
        let (scope, key) = registry.add(name.clone()).unwrap().into_parts();
        let authority = registry.authenticate(&scope, &key).unwrap();
        let contender = registry.clone();
        let (begin, start) = mpsc::channel();
        let (entered, started) = mpsc::channel();
        let (done, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            if start
                .recv_timeout(std::time::Duration::from_secs(1))
                .is_err()
            {
                return;
            }
            entered.send(()).unwrap();
            done.send(contender.revoke(&name)).unwrap();
        });
        let published = registry.try_with_active_scope(&scope, || {
            // Test-only rendezvous deliberately holds the real gate long
            // enough to observe a competing real revoke. Production callbacks
            // must never wait or perform I/O.
            begin.send(()).unwrap();
            started
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            assert!(matches!(
                completion.recv_timeout(std::time::Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            assert!(!authority.is_revoked());
            7
        });
        drop(begin);
        let revoked = completion.recv_timeout(std::time::Duration::from_secs(1));
        worker.join().unwrap();
        assert_eq!(published, Ok(7));
        assert_eq!(revoked.unwrap(), Ok(scope.clone()));
        assert!(authority.is_revoked());
        assert_eq!(
            registry.try_with_active_scope::<()>(&scope, || panic!("must not publish")),
            Err(RegistryError::Unauthorized)
        );
    }

    #[test]
    fn malformed_duplicate_and_zero_epoch_snapshots_are_refused() {
        let home = home();
        crate::platform::prepare_secret_home(home.path()).unwrap();
        let record = Record {
            name: "box-a".into(),
            epoch: 1,
            digest: [1; 32],
            revoked: true,
        };
        for records in [
            vec![record.clone(), record.clone()],
            vec![Record {
                epoch: 0,
                ..record.clone()
            }],
            vec![Record {
                name: "../bad".into(),
                ..record.clone()
            }],
        ] {
            write_snapshot(
                &home.path().join(FILE_NAME),
                &serde_json::to_vec(&Snapshot {
                    version: 1,
                    boxes: records,
                })
                .unwrap(),
            )
            .unwrap();
            assert!(matches!(
                BoxRegistry::open(home.path()),
                Err(RegistryError::InvalidData)
            ));
        }
    }
}
