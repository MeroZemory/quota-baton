//! Process-local launch handles. Authentication and binding resolution belong to callers.
//! Hold a `LaunchPin` until the response body/stream (or WebSocket) finishes.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock, Weak,
    atomic::{AtomicBool, AtomicI64, Ordering},
};
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::TryRngCore;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub const CAPACITY: usize = 4096;
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
pub const MAX_ANCHOR_BYTES: usize = 256;
pub const MAX_ADVERTISED_MODELS: usize = 2048;
pub const MAX_MODEL_ID_BYTES: usize = 256;
pub const MAX_ADVERTISED_BYTES: usize = 64 * 1024;
const GENERATION_ATTEMPTS: usize = 8;
type TokenDigest = [u8; 32];
type Clock = dyn Fn() -> Instant + Send + Sync;

/// The sole account authority is the binding addressed by `binding_anchor_key`.
/// Callers supply a nonsecret internal identifier, never credentials or a URL.
/// Accepted anchors contain only ASCII letters, digits, `-`, `_`, `.` and `:`.
#[derive(Clone, PartialEq, Eq)]
pub struct LaunchMetadata {
    pub binding_anchor_key: String,
    pub advertised_models: BTreeSet<String>,
}

impl fmt::Debug for LaunchMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LaunchMetadata([REDACTED])")
    }
}

fn validate_anchor(anchor: &str) -> Result<(), LaunchTokenError> {
    if anchor.is_empty()
        || anchor.len() > MAX_ANCHOR_BYTES
        || !anchor
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err(LaunchTokenError::InvalidMetadata);
    }
    Ok(())
}

fn bounded_models(models: BTreeSet<String>) -> Result<BTreeSet<String>, LaunchTokenError> {
    if models.len() > MAX_ADVERTISED_MODELS {
        return Err(LaunchTokenError::InvalidMetadata);
    }
    let mut bytes = 0usize;
    for model in &models {
        if model.is_empty()
            || model.len() > MAX_MODEL_ID_BYTES
            || model.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(LaunchTokenError::InvalidMetadata);
        }
        bytes = bytes
            .checked_add(model.len())
            .ok_or(LaunchTokenError::InvalidMetadata)?;
        if bytes > MAX_ADVERTISED_BYTES {
            return Err(LaunchTokenError::InvalidMetadata);
        }
    }
    // Discard excess caller allocation capacity as well as bounding text lengths.
    Ok(models
        .into_iter()
        .map(|model| model.into_boxed_str().into_string())
        .collect())
}

/// Only issuance returns plaintext. No Display, serialization, or implicit dereference.
pub struct LaunchToken(Zeroizing<String>);

impl LaunchToken {
    /// For the launch URL only. Callers must redact URL paths before logging them.
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for LaunchToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LaunchToken([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LaunchTokenError {
    #[error("invalid launch metadata")]
    InvalidMetadata,
    #[error("launch token missing")]
    Missing,
    #[error("launch token expired")]
    Expired,
    #[error("launch token registry at capacity")]
    Capacity,
    #[error("secure launch token generation failed")]
    Entropy,
    #[error("launch token has too many active requests")]
    PinCapacity,
}

struct Entry {
    metadata: LaunchMetadata,
    last_activity: Instant,
    pins: usize,
    deferred_owner: Weak<DeferredOwner>,
    binding_activity: Arc<AtomicI64>,
}

impl Entry {
    fn expired(&self, now: Instant) -> bool {
        self.pins == 0
            && self
                .deferred_owner
                .upgrade()
                .is_none_or(|owner| !owner.live.load(Ordering::Acquire))
            && now.saturating_duration_since(self.last_activity) >= IDLE_TIMEOUT
    }
}

struct Inner {
    entries: Mutex<HashMap<TokenDigest, Entry>>,
    clock: Arc<Clock>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, HashMap<TokenDigest, Entry>> {
        // No user code mutates the map; recover ownership if a thread unwinds.
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone)]
pub struct LaunchTokenRegistry(Arc<Inner>);

impl fmt::Debug for LaunchTokenRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LaunchTokenRegistry { .. }")
    }
}

impl Default for LaunchTokenRegistry {
    fn default() -> Self {
        Self::with_clock(Arc::new(Instant::now))
    }
}

impl LaunchTokenRegistry {
    fn with_clock(clock: Arc<Clock>) -> Self {
        Self(Arc::new(Inner {
            entries: Mutex::new(HashMap::new()),
            clock,
        }))
    }

    pub fn issue(&self, metadata: LaunchMetadata) -> Result<LaunchToken, LaunchTokenError> {
        self.issue_with(metadata, || {
            let mut bytes = Zeroizing::new([0u8; 32]);
            rand::rngs::OsRng
                .try_fill_bytes(&mut *bytes)
                .map_err(|_| LaunchTokenError::Entropy)?;
            Ok(bytes)
        })
    }

    // The entropy seam is private: deterministic entropy can never be selected by callers.
    fn issue_with(
        &self,
        metadata: LaunchMetadata,
        mut entropy: impl FnMut() -> Result<Zeroizing<[u8; 32]>, LaunchTokenError>,
    ) -> Result<LaunchToken, LaunchTokenError> {
        validate_anchor(&metadata.binding_anchor_key)?;
        let metadata = LaunchMetadata {
            binding_anchor_key: metadata.binding_anchor_key.into_boxed_str().into_string(),
            advertised_models: bounded_models(metadata.advertised_models)?,
        };
        {
            let mut entries = self.0.lock();
            let now = (self.0.clock)();
            entries.retain(|_, entry| !entry.expired(now));
            if entries.len() >= CAPACITY {
                return Err(LaunchTokenError::Capacity);
            }
        }
        // Entropy acquisition must not block pin acquisition or stream completion.
        // Bound retries even if the entropy provider repeatedly returns a collision.
        for _ in 0..GENERATION_ATTEMPTS {
            let bytes = entropy()?;
            let token = LaunchToken(Zeroizing::new(URL_SAFE_NO_PAD.encode(bytes.as_slice())));
            let digest = token_digest(token.expose_secret());
            let mut entries = self.0.lock();
            let now = (self.0.clock)();
            entries.retain(|_, entry| !entry.expired(now));
            // Recheck atomically: another issuer may have consumed the final slot.
            if entries.len() >= CAPACITY {
                return Err(LaunchTokenError::Capacity);
            }
            if let std::collections::hash_map::Entry::Vacant(slot) = entries.entry(digest) {
                slot.insert(Entry {
                    metadata,
                    last_activity: now,
                    pins: 0,
                    deferred_owner: Weak::new(),
                    binding_activity: Arc::new(AtomicI64::new(
                        chrono::Utc::now().timestamp_millis(),
                    )),
                });
                return Ok(token);
            }
        }
        Err(LaunchTokenError::Entropy)
    }

    /// Issuance-only cleanup: caller owns a never-published handle and has
    /// dropped its private pin. Do not use this to revoke a delivered launch.
    pub(crate) fn discard_unpublished(&self, token: &LaunchToken) {
        self.0.lock().remove(&token_digest(token.expose_secret()));
    }

    /// Attach only after authoritative placement, on the blocking issue worker.
    /// Release the registry lock before binding registration. Pins thereafter
    /// publish activity atomically without acquiring any binding locks.
    pub(crate) fn bind_activity(
        &self,
        token: &LaunchToken,
        table: &Arc<crate::bindings::BindingTable>,
    ) {
        let activity = self
            .0
            .lock()
            .get(&token_digest(token.expose_secret()))
            .map(|entry| {
                (
                    entry.metadata.binding_anchor_key.clone(),
                    entry.binding_activity.clone(),
                )
            });
        if let Some((anchor, stamp)) = activity {
            table.bind_launch_activity(&anchor, &stamp);
        }
    }

    /// Atomically validate and pin. Each request obtains its own non-cloneable guard.
    /// Missing/expired failures are for the caller to apply only when a token is needed.
    pub fn get_or_pin(&self, token: &str) -> Result<LaunchPin, LaunchTokenError> {
        let digest = incoming_digest(token)?;
        let mut entries = self.0.lock();
        let now = (self.0.clock)();
        let entry = live_entry(&mut entries, &digest, now)?;
        entry.pins = entry
            .pins
            .checked_add(1)
            .ok_or(LaunchTokenError::PinCapacity)?;
        entry
            .binding_activity
            .fetch_max(chrono::Utc::now().timestamp_millis(), Ordering::Release);
        Ok(LaunchPin {
            inner: Arc::clone(&self.0),
            digest,
        })
    }

    /// Replace only the advertised model set. Maintenance does not refresh idle time.
    /// The binding anchor cannot be changed through this API.
    pub fn update_advertised(
        &self,
        token: &str,
        models: BTreeSet<String>,
    ) -> Result<(), LaunchTokenError> {
        let digest = incoming_digest(token)?;
        let models = bounded_models(models)?;
        let mut entries = self.0.lock();
        let now = (self.0.clock)();
        live_entry(&mut entries, &digest, now)?
            .metadata
            .advertised_models = models;
        Ok(())
    }
}

fn token_digest(token: &str) -> TokenDigest {
    Sha256::digest(token.as_bytes()).into()
}

fn incoming_digest(token: &str) -> Result<TokenDigest, LaunchTokenError> {
    // Reject unbounded attacker-controlled strings before hashing or locking.
    if token.len() != 43
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(LaunchTokenError::Missing);
    }
    Ok(token_digest(token))
}

fn live_entry<'a>(
    entries: &'a mut HashMap<TokenDigest, Entry>,
    digest: &TokenDigest,
    now: Instant,
) -> Result<&'a mut Entry, LaunchTokenError> {
    let entry = entries.get_mut(digest).ok_or(LaunchTokenError::Missing)?;
    if entry.expired(now) {
        return Err(LaunchTokenError::Expired);
    }
    Ok(entry)
}

/// An active request pins an entry across expiry and capacity reclamation.
/// Drop on completion, cancellation, or unwind refreshes the idle timestamp.
#[must_use = "hold the pin until the entire request/response stream completes"]
pub struct LaunchPin {
    inner: Arc<Inner>,
    digest: TokenDigest,
}

impl fmt::Debug for LaunchPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LaunchPin { .. }")
    }
}

/// A control connection's process-local lifetime, independent of request pins.
/// Only a real issued pin can create this owner; it grants no account authority.
pub(crate) struct LaunchOwnerLease {
    scope: LaunchOwnerScope,
}
#[derive(Clone)]
pub(crate) struct LaunchOwnerScope(Arc<DeferredOwner>);
struct DeferredOwner {
    registry: Weak<Inner>,
    digest: TokenDigest,
    anchor: String,
    root: OnceLock<String>,
    reset_runtime: OnceLock<Weak<()>>,
    episode: Mutex<OwnerEpisode>,
    live: AtomicBool,
    gate: Mutex<()>,
    changed: tokio::sync::Notify,
}
// Constant-size owner receipt survives reclamation of terminal reset records.
// Only an explicit advance can reopen registration; the root is never cleared.
enum OwnerEpisode {
    Open {
        previous: u64,
        after: Option<chrono::DateTime<chrono::Utc>>,
    },
    Registered(u64),
    Consumed {
        id: u64,
        at: chrono::DateTime<chrono::Utc>,
        pending: Option<u64>,
    },
}
impl LaunchOwnerLease {
    pub(crate) fn scope(&self) -> LaunchOwnerScope {
        self.scope.clone()
    }
}
impl Drop for LaunchOwnerLease {
    fn drop(&mut self) {
        let _gate = self.scope.0.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.scope.0.live.store(false, Ordering::Release);
        self.scope.0.changed.notify_waiters();
    }
}
impl LaunchOwnerScope {
    pub(crate) fn live_guard(&self) -> Option<MutexGuard<'_, ()>> {
        let guard = self.0.gate.lock().ok()?;
        self.live().then_some(guard)
    }
    pub(crate) fn live(&self) -> bool {
        self.0.live.load(Ordering::Acquire)
    }
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn anchor(&self) -> &str {
        &self.0.anchor
    }
    pub(crate) fn root(&self) -> Option<&str> {
        self.0.root.get().map(String::as_str)
    }
    pub(crate) fn matches_pin(&self, pin: &LaunchPin) -> bool {
        self.live()
            && self.0.digest == pin.digest
            && Weak::ptr_eq(&self.0.registry, &Arc::downgrade(&pin.inner))
    }
    pub(crate) fn claim(
        &self,
        root: &str,
        id: u64,
        source_started_at: chrono::DateTime<chrono::Utc>,
        runtime: &Arc<()>,
    ) -> bool {
        let Ok(mut episode) = self.0.episode.lock() else {
            return false;
        };
        let (previous, after) = match *episode {
            OwnerEpisode::Open { previous, after } => (previous, after),
            OwnerEpisode::Consumed {
                id,
                at,
                pending: None,
            } => (id, Some(at)),
            _ => return false,
        };
        if !self.live()
            || id <= previous
            || after.is_some_and(|after| source_started_at <= after)
            || self.root().is_some_and(|bound| bound != root)
            || self
                .0
                .reset_runtime
                .get()
                .is_some_and(|bound| !Weak::ptr_eq(bound, &Arc::downgrade(runtime)))
        {
            return false;
        }
        if self.root().is_none() && self.0.root.set(root.to_owned()).is_err() {
            return false;
        }
        if self.0.reset_runtime.get().is_none()
            && self.0.reset_runtime.set(Arc::downgrade(runtime)).is_err()
        {
            return false;
        }
        if let OwnerEpisode::Consumed { pending, .. } = &mut *episode {
            // One genuinely new qualified registration may arrive before its
            // parent processes turn progress. It remains unusable until Advance.
            *pending = Some(id);
        } else {
            *episode = OwnerEpisode::Registered(id);
        }
        self.live()
    }
    pub(crate) fn episode_id(&self) -> Result<Option<u64>, ()> {
        Ok(match *self.0.episode.lock().map_err(|_| ())? {
            OwnerEpisode::Open { .. } => None,
            OwnerEpisode::Registered(id) | OwnerEpisode::Consumed { id, .. } => Some(id),
        })
    }
    /// Called under the reset record and owner lifetime gates, only after the
    /// final READY -> CONSUMED transition. No retained record is needed later.
    pub(crate) fn consumed_episode(&self, id: u64) -> bool {
        let Ok(mut episode) = self.0.episode.lock() else {
            return false;
        };
        if !self.live() || !matches!(*episode, OwnerEpisode::Registered(actual) if actual == id) {
            return false;
        }
        *episode = OwnerEpisode::Consumed {
            id,
            at: chrono::Utc::now(),
            pending: None,
        };
        true
    }
    pub(crate) fn pending_episode(&self, id: u64, runtime: &Arc<()>) -> Result<Option<u64>, ()> {
        let episode = self.0.episode.lock().map_err(|_| ())?;
        if !self.live()
            || !self
                .0
                .reset_runtime
                .get()
                .is_some_and(|bound| Weak::ptr_eq(bound, &Arc::downgrade(runtime)))
        {
            return Err(());
        }
        match *episode {
            OwnerEpisode::Consumed {
                id: actual,
                pending,
                ..
            } if actual == id => Ok(pending),
            _ => Err(()),
        }
    }
    pub(crate) fn advance_episode(
        &self,
        id: u64,
        runtime: &Arc<()>,
        expected: Option<u64>,
    ) -> bool {
        let Ok(mut episode) = self.0.episode.lock() else {
            return false;
        };
        let OwnerEpisode::Consumed {
            id: actual,
            at,
            pending,
        } = *episode
        else {
            return false;
        };
        if !self.live()
            || actual != id
            || pending != expected
            || !self
                .0
                .reset_runtime
                .get()
                .is_some_and(|bound| Weak::ptr_eq(bound, &Arc::downgrade(runtime)))
        {
            return false;
        }
        *episode = if let Some(pending) = pending {
            OwnerEpisode::Registered(pending)
        } else {
            OwnerEpisode::Open {
                previous: id,
                after: Some(at),
            }
        };
        true
    }
    pub(crate) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.0.changed.notified()
    }
    pub(crate) fn matches_thread(&self, thread: &str) -> bool {
        self.live()
            && valid_native_thread(thread)
            && self.root().and_then(|root| root.strip_prefix("codex:")) == Some(thread)
    }
}
pub(crate) fn valid_native_thread(thread: &str) -> bool {
    thread.len() == 36
        && thread.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

impl LaunchPin {
    pub(crate) fn deferred_owner_scope(&self) -> Option<LaunchOwnerScope> {
        let owner = self
            .inner
            .lock()
            .get(&self.digest)?
            .deferred_owner
            .upgrade()?;
        owner
            .live
            .load(Ordering::Acquire)
            .then_some(LaunchOwnerScope(owner))
    }
    /// Open before data ingress. Registration later binds the observed C8 UUID.
    pub(crate) fn open_deferred_owner(&self) -> Result<LaunchOwnerLease, LaunchTokenError> {
        let mut entries = self.inner.lock();
        let entry = entries
            .get_mut(&self.digest)
            .ok_or(LaunchTokenError::Missing)?;
        if entry
            .deferred_owner
            .upgrade()
            .is_some_and(|owner| owner.live.load(Ordering::Acquire))
        {
            return Err(LaunchTokenError::Capacity);
        }
        let owner = Arc::new(DeferredOwner {
            registry: Arc::downgrade(&self.inner),
            digest: self.digest,
            anchor: entry.metadata.binding_anchor_key.clone(),
            root: OnceLock::new(),
            reset_runtime: OnceLock::new(),
            episode: Mutex::new(OwnerEpisode::Open {
                previous: 0,
                after: None,
            }),
            live: AtomicBool::new(true),
            gate: Mutex::new(()),
            changed: tokio::sync::Notify::new(),
        });
        entry.deferred_owner = Arc::downgrade(&owner);
        Ok(LaunchOwnerLease {
            scope: LaunchOwnerScope(owner),
        })
    }
    /// A fresh snapshot, including catalog updates made after pin acquisition.
    pub fn metadata(&self) -> LaunchMetadata {
        self.inner.lock()[&self.digest].metadata.clone()
    }
}

impl Drop for LaunchPin {
    fn drop(&mut self) {
        let mut entries = self.inner.lock();
        if let Some(entry) = entries.get_mut(&self.digest) {
            entry.pins -= 1;
            entry.last_activity = entry.last_activity.max((self.inner.clock)());
            entry
                .binding_activity
                .fetch_max(chrono::Utc::now().timestamp_millis(), Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    // Synchronous injected monotonic time: no sleeping, runtime, or test-util needed.
    struct Fixture {
        registry: LaunchTokenRegistry,
        seconds: Arc<AtomicU64>,
        sequence: AtomicU64,
    }

    impl Fixture {
        fn new() -> Self {
            let origin = Instant::now();
            let seconds = Arc::new(AtomicU64::new(0));
            let clock_seconds = Arc::clone(&seconds);
            Self {
                registry: LaunchTokenRegistry::with_clock(Arc::new(move || {
                    origin + Duration::from_secs(clock_seconds.load(Ordering::SeqCst))
                })),
                seconds,
                sequence: AtomicU64::new(0),
            }
        }

        fn advance(&self, seconds: u64) {
            self.seconds.fetch_add(seconds, Ordering::SeqCst);
        }

        fn issue(&self) -> LaunchToken {
            self.registry
                .issue_with(metadata(), || {
                    let n = self.sequence.fetch_add(1, Ordering::SeqCst);
                    Ok(Zeroizing::new(Sha256::digest(n.to_le_bytes()).into()))
                })
                .unwrap()
        }
    }

    fn metadata() -> LaunchMetadata {
        LaunchMetadata {
            binding_anchor_key: "binding-anchor".into(),
            advertised_models: BTreeSet::from(["model-a".into()]),
        }
    }

    #[test]
    fn deferred_owner_advance_preserves_root_pin_runtime_and_requires_new_source() {
        let f = Fixture::new();
        let token = f.issue();
        let pin = f.registry.get_or_pin(token.expose_secret()).unwrap();
        let lease = pin.open_deferred_owner().unwrap();
        let scope = lease.scope();
        let runtime = Arc::new(());
        let foreign_runtime = Arc::new(());
        let root = "codex:11111111-2222-4333-8444-555555555555";
        let old_source = chrono::Utc::now();
        assert!(!scope.advance_episode(7, &runtime, None));
        assert!(!scope.claim(root, 0, old_source, &runtime));
        assert!(scope.claim(root, 7, old_source, &runtime));
        assert!(!scope.advance_episode(7, &runtime, None));
        assert!(!scope.consumed_episode(8));
        assert!(scope.consumed_episode(7));
        // Consumption alone never makes another episode current.
        assert!(!scope.claim(root, 8, old_source, &runtime));
        assert!(!scope.advance_episode(8, &runtime, None));
        assert!(!scope.advance_episode(7, &foreign_runtime, None));
        assert!(scope.advance_episode(7, &runtime, None));
        assert!(!scope.advance_episode(7, &runtime, None));
        assert_eq!(scope.episode_id(), Ok(None));
        assert_eq!(scope.root(), Some(root));
        assert!(scope.matches_pin(&pin));
        let other_token = f.issue();
        let other_pin = f.registry.get_or_pin(other_token.expose_secret()).unwrap();
        assert!(!scope.matches_pin(&other_pin));

        let cutoff = match *scope.0.episode.lock().unwrap() {
            OwnerEpisode::Open {
                after: Some(at), ..
            } => at,
            _ => panic!("advance must open one fresh registration"),
        };
        let fresh = cutoff + chrono::Duration::nanoseconds(1);
        assert!(!scope.claim(root, 8, old_source, &runtime));
        assert!(!scope.claim(root, 8, cutoff, &runtime));
        assert!(!scope.claim(root, 7, fresh, &runtime));
        assert!(!scope.claim("codex:other-root", 8, fresh, &runtime));
        assert!(!scope.claim(root, 8, fresh, &foreign_runtime));
        assert!(scope.claim(root, 8, fresh, &runtime));
        assert_eq!(scope.episode_id(), Ok(Some(8)));
        assert!(!scope.consumed_episode(7));
        assert!(!scope.advance_episode(7, &runtime, None));
        assert!(scope.consumed_episode(8));
        drop(lease);
        assert!(!scope.advance_episode(8, &runtime, None));
        assert!(!scope.claim(root, 9, fresh, &runtime));
    }

    #[test]
    fn deferred_owner_has_only_one_pending_registration_until_exact_advance() {
        let f = Fixture::new();
        let token = f.issue();
        let pin = f.registry.get_or_pin(token.expose_secret()).unwrap();
        let lease = pin.open_deferred_owner().unwrap();
        let scope = lease.scope();
        let runtime = Arc::new(());
        let root = "codex:11111111-2222-4333-8444-555555555555";
        assert!(scope.claim(root, 1, chrono::Utc::now(), &runtime));
        assert!(scope.consumed_episode(1));
        let cutoff = match *scope.0.episode.lock().unwrap() {
            OwnerEpisode::Consumed { at, .. } => at,
            _ => panic!("missing exact consumed receipt"),
        };
        let fresh = cutoff + chrono::Duration::nanoseconds(1);
        assert!(!scope.claim(root, 2, cutoff, &runtime));
        assert!(scope.claim(root, 2, fresh, &runtime));
        assert!(!scope.claim(root, 3, fresh, &runtime));
        assert_eq!(scope.episode_id(), Ok(Some(1)));
        assert!(!scope.consumed_episode(2));
        assert!(!scope.advance_episode(1, &runtime, None));
        assert!(!scope.advance_episode(1, &runtime, Some(3)));
        assert!(scope.advance_episode(1, &runtime, Some(2)));
        assert_eq!(scope.episode_id(), Ok(Some(2)));
        assert!(!scope.advance_episode(1, &runtime, Some(2)));
    }

    #[test]
    fn invalid_anchors_fail_before_entropy_or_registry_mutation() {
        let f = Fixture::new();
        for anchor in [
            String::new(),
            "x".repeat(MAX_ANCHOR_BYTES + 1),
            "bad anchor".into(),
            "bad\nanchor".into(),
            "https://example.invalid/secret".into(),
            "é".into(),
        ] {
            let mut data = metadata();
            data.binding_anchor_key = anchor;
            assert_eq!(
                f.registry
                    .issue_with(data, || panic!("invalid metadata reached entropy"))
                    .unwrap_err(),
                LaunchTokenError::InvalidMetadata
            );
        }
        assert!(f.registry.0.lock().is_empty());
        let mut data = metadata();
        data.binding_anchor_key = "a".repeat(MAX_ANCHOR_BYTES);
        let token = f
            .registry
            .issue_with(data.clone(), || Ok(Zeroizing::new([7; 32])))
            .unwrap();
        assert_eq!(
            f.registry
                .get_or_pin(token.expose_secret())
                .unwrap()
                .metadata(),
            data
        );
        assert_eq!(format!("{data:?}"), "LaunchMetadata([REDACTED])");
    }

    #[test]
    fn model_metadata_limits_apply_to_issue_and_update_atomically() {
        let f = Fixture::new();
        let token = f.issue();
        let mut over_bytes: BTreeSet<_> = (0..MAX_ADVERTISED_BYTES / MAX_MODEL_ID_BYTES)
            .map(|n| format!("{n:04}{}", "x".repeat(MAX_MODEL_ID_BYTES - 4)))
            .collect();
        over_bytes.insert("extra".into());
        for models in [
            BTreeSet::from([String::new()]),
            BTreeSet::from(["bad model".into()]),
            BTreeSet::from(["bad\nmodel".into()]),
            BTreeSet::from(["x".repeat(MAX_MODEL_ID_BYTES + 1)]),
            (0..=MAX_ADVERTISED_MODELS)
                .map(|n| format!("m{n}"))
                .collect(),
            over_bytes,
        ] {
            assert_eq!(
                f.registry
                    .issue_with(
                        LaunchMetadata {
                            binding_anchor_key: "valid-anchor".into(),
                            advertised_models: models.clone(),
                        },
                        || panic!("invalid metadata reached entropy"),
                    )
                    .unwrap_err(),
                LaunchTokenError::InvalidMetadata
            );
            assert_eq!(
                f.registry.update_advertised(token.expose_secret(), models),
                Err(LaunchTokenError::InvalidMetadata)
            );
        }
        assert_eq!(f.registry.0.lock().len(), 1);
        let entry_digest = token_digest(token.expose_secret());
        assert_eq!(f.registry.0.lock()[&entry_digest].metadata, metadata());
        f.advance(IDLE_TIMEOUT.as_secs());
        assert_eq!(
            f.registry.get_or_pin(token.expose_secret()).unwrap_err(),
            LaunchTokenError::Expired
        );
    }

    #[test]
    fn exact_model_limits_and_empty_catalog_are_accepted_without_excess_capacity() {
        let f = Fixture::new();
        let mut anchor = String::with_capacity(MAX_ANCHOR_BYTES * 4);
        anchor.push_str("anchor:1");
        let mut model = String::with_capacity(MAX_ADVERTISED_BYTES * 2);
        model.push_str("model-a");
        let token = f
            .registry
            .issue_with(
                LaunchMetadata {
                    binding_anchor_key: anchor,
                    advertised_models: BTreeSet::from([model]),
                },
                || Ok(Zeroizing::new([8; 32])),
            )
            .unwrap();
        {
            let entries = f.registry.0.lock();
            let data = &entries[&token_digest(token.expose_secret())].metadata;
            assert_eq!(
                data.binding_anchor_key.capacity(),
                data.binding_anchor_key.len()
            );
            let model = data.advertised_models.first().unwrap();
            assert_eq!(model.capacity(), model.len());
        }
        for models in [
            (0..MAX_ADVERTISED_MODELS)
                .map(|n| format!("m{n}"))
                .collect(),
            (0..MAX_ADVERTISED_BYTES / MAX_MODEL_ID_BYTES)
                .map(|n| format!("{n:04}{}", "x".repeat(MAX_MODEL_ID_BYTES - 4)))
                .collect(),
            BTreeSet::new(),
        ] {
            f.registry
                .update_advertised(token.expose_secret(), models.clone())
                .unwrap();
            assert_eq!(
                f.registry
                    .get_or_pin(token.expose_secret())
                    .unwrap()
                    .metadata()
                    .advertised_models,
                models
            );
        }
    }

    #[test]
    fn exact_idle_boundary_and_explicit_errors() {
        let f = Fixture::new();
        let token = f.issue();
        assert_eq!(
            f.registry.get_or_pin("unknown").unwrap_err(),
            LaunchTokenError::Missing
        );
        f.advance(IDLE_TIMEOUT.as_secs() - 1);
        let pin = f.registry.get_or_pin(token.expose_secret()).unwrap();
        assert_eq!(pin.metadata(), metadata());
        drop(pin);
        f.advance(IDLE_TIMEOUT.as_secs());
        assert_eq!(
            f.registry.get_or_pin(token.expose_secret()).unwrap_err(),
            LaunchTokenError::Expired
        );
        assert_eq!(
            f.registry
                .update_advertised(token.expose_secret(), BTreeSet::new()),
            Err(LaunchTokenError::Expired)
        );
        assert_eq!(
            f.registry.update_advertised("unknown", BTreeSet::new()),
            Err(LaunchTokenError::Missing)
        );
    }

    #[test]
    fn multiple_pins_survive_long_requests_and_each_completion_touches() {
        let f = Fixture::new();
        let token = f.issue();
        let first = f.registry.get_or_pin(token.expose_secret()).unwrap();
        f.advance(IDLE_TIMEOUT.as_secs() * 3);
        let second = f.registry.get_or_pin(token.expose_secret()).unwrap();
        drop(first);
        f.advance(IDLE_TIMEOUT.as_secs() * 2);
        assert_eq!(second.metadata(), metadata());
        drop(second);
        f.advance(IDLE_TIMEOUT.as_secs() - 1);
        let third = f.registry.get_or_pin(token.expose_secret()).unwrap();
        drop(third);
        f.advance(IDLE_TIMEOUT.as_secs());
        assert_eq!(
            f.registry.get_or_pin(token.expose_secret()).unwrap_err(),
            LaunchTokenError::Expired
        );
    }

    #[test]
    fn catalog_update_preserves_anchor_and_does_not_extend_idle_life() {
        let f = Fixture::new();
        let token = f.issue();
        let pin = f.registry.get_or_pin(token.expose_secret()).unwrap();
        let models = BTreeSet::from(["model-b".into(), "model-c".into()]);
        f.registry
            .update_advertised(token.expose_secret(), models.clone())
            .unwrap();
        assert_eq!(pin.metadata().binding_anchor_key, "binding-anchor");
        assert_eq!(pin.metadata().advertised_models, models);
        drop(pin);
        f.advance(IDLE_TIMEOUT.as_secs() - 1);
        f.registry
            .update_advertised(token.expose_secret(), BTreeSet::new())
            .unwrap();
        f.advance(1);
        assert_eq!(
            f.registry.get_or_pin(token.expose_secret()).unwrap_err(),
            LaunchTokenError::Expired
        );
    }

    #[test]
    fn capacity_reclaims_only_expired_unpinned_entries() {
        let f = Fixture::new();
        let pinned_token = f.issue();
        let pin = f.registry.get_or_pin(pinned_token.expose_secret()).unwrap();
        let expired_token = f.issue();
        for _ in 2..CAPACITY {
            f.issue();
        }
        assert_eq!(
            f.registry.issue(metadata()).unwrap_err(),
            LaunchTokenError::Capacity
        );
        f.advance(IDLE_TIMEOUT.as_secs());
        let replacement = f.issue();
        assert_eq!(f.registry.0.lock().len(), 2);
        assert_eq!(pin.metadata(), metadata());
        assert!(f.registry.get_or_pin(replacement.expose_secret()).is_ok());
        // Pruned handles have no unbounded tombstone/history map.
        assert_eq!(
            f.registry
                .get_or_pin(expired_token.expose_secret())
                .unwrap_err(),
            LaunchTokenError::Missing
        );
    }

    #[test]
    fn all_pinned_entries_still_consume_capacity_after_timeout() {
        let f = Fixture::new();
        let pins: Vec<_> = (0..CAPACITY)
            .map(|_| f.registry.get_or_pin(f.issue().expose_secret()).unwrap())
            .collect();
        f.advance(IDLE_TIMEOUT.as_secs() * 2);
        assert_eq!(
            f.registry.issue(metadata()).unwrap_err(),
            LaunchTokenError::Capacity
        );
        drop(pins);
        assert_eq!(
            f.registry.issue(metadata()).unwrap_err(),
            LaunchTokenError::Capacity
        );
        f.advance(IDLE_TIMEOUT.as_secs());
        f.issue();
        assert_eq!(f.registry.0.lock().len(), 1);
    }

    #[test]
    fn digest_index_redaction_and_entropy_failure() {
        let f = Fixture::new();
        let token = f.issue();
        let decoded = Zeroizing::new(URL_SAFE_NO_PAD.decode(token.expose_secret()).unwrap());
        assert_eq!(decoded.len(), 32);
        assert_eq!(token.expose_secret().len(), 43);
        let pin = f.registry.get_or_pin(token.expose_secret()).unwrap();
        assert_eq!(format!("{token:?}"), "LaunchToken([REDACTED])");
        assert!(!format!("{:?} {pin:?}", f.registry).contains(token.expose_secret()));
        assert!(
            f.registry
                .0
                .lock()
                .contains_key(&token_digest(token.expose_secret()))
        );
        assert_eq!(
            f.registry
                .issue_with(metadata(), || Err(LaunchTokenError::Entropy))
                .unwrap_err(),
            LaunchTokenError::Entropy
        );
        assert_eq!(f.registry.0.lock().len(), 1);
    }

    #[test]
    fn collision_retries_without_overwriting_existing_metadata() {
        let f = Fixture::new();
        let first = f.issue();
        let mut calls = 0u64;
        let mut other_metadata = metadata();
        other_metadata.binding_anchor_key = "another-anchor".into();
        let second = f
            .registry
            .issue_with(other_metadata.clone(), || {
                let bytes = Sha256::digest(calls.to_le_bytes()).into();
                calls += 1;
                Ok(Zeroizing::new(bytes))
            })
            .unwrap();
        assert_eq!(calls, 2);
        assert_ne!(first.expose_secret(), second.expose_secret());
        assert_eq!(
            f.registry
                .get_or_pin(first.expose_secret())
                .unwrap()
                .metadata(),
            metadata()
        );
        assert_eq!(
            f.registry
                .get_or_pin(second.expose_secret())
                .unwrap()
                .metadata(),
            other_metadata
        );
    }

    #[test]
    fn repeated_collisions_are_bounded_and_do_not_hold_the_registry_lock() {
        let f = Fixture::new();
        let first = f.issue();
        let mut calls = 0;
        let result = f.registry.issue_with(metadata(), || {
            calls += 1;
            // Reentering the registry proves entropy runs outside its mutex.
            let pin = f.registry.get_or_pin(first.expose_secret()).unwrap();
            assert_eq!(pin.metadata(), metadata());
            Ok(Zeroizing::new(Sha256::digest(0u64.to_le_bytes()).into()))
        });
        assert_eq!(result.unwrap_err(), LaunchTokenError::Entropy);
        assert_eq!(calls, GENERATION_ATTEMPTS);
        assert_eq!(f.registry.0.lock().len(), 1);
    }

    #[test]
    fn malformed_handles_fail_without_touching_live_entries() {
        let f = Fixture::new();
        let token = f.issue();
        for invalid in [
            String::new(),
            "x".repeat(4096),
            "/".repeat(43),
            "é".repeat(43),
        ] {
            assert_eq!(
                f.registry.get_or_pin(&invalid).unwrap_err(),
                LaunchTokenError::Missing
            );
            assert_eq!(
                f.registry.update_advertised(&invalid, BTreeSet::new()),
                Err(LaunchTokenError::Missing)
            );
        }
        assert_eq!(
            f.registry
                .get_or_pin(token.expose_secret())
                .unwrap()
                .metadata(),
            metadata()
        );
    }

    #[test]
    fn pin_overflow_is_explicit_and_does_not_wrap() {
        let f = Fixture::new();
        let token = f.issue();
        let digest = token_digest(token.expose_secret());
        f.registry.0.lock().get_mut(&digest).unwrap().pins = usize::MAX;
        assert_eq!(
            f.registry.get_or_pin(token.expose_secret()).unwrap_err(),
            LaunchTokenError::PinCapacity
        );
        assert_eq!(f.registry.0.lock()[&digest].pins, usize::MAX);
        f.registry.0.lock().get_mut(&digest).unwrap().pins = 0;
    }

    #[test]
    fn pins_keep_registry_alive_after_all_registry_handles_are_dropped() {
        let f = Fixture::new();
        let token = f.issue();
        let pin = f.registry.get_or_pin(token.expose_secret()).unwrap();
        drop(f);
        assert_eq!(pin.metadata(), metadata());
        drop(pin);
    }

    #[test]
    fn concurrent_issuers_cannot_overfill_the_last_slot() {
        let f = Arc::new(Fixture::new());
        for _ in 0..CAPACITY - 1 {
            f.issue();
        }
        let ready = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2u64)
            .map(|n| {
                let f = Arc::clone(&f);
                let ready = Arc::clone(&ready);
                thread::spawn(move || {
                    f.registry.issue_with(metadata(), || {
                        // Both issuers pass preflight before either inserts its candidate.
                        ready.wait();
                        Ok(Zeroizing::new(
                            Sha256::digest((CAPACITY as u64 + n).to_le_bytes()).into(),
                        ))
                    })
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(LaunchTokenError::Capacity)))
                .count(),
            1
        );
        assert_eq!(f.registry.0.lock().len(), CAPACITY);
    }

    #[test]
    fn concurrent_pins_updates_and_issuance_share_one_registry() {
        let f = Arc::new(Fixture::new());
        let token = Arc::new(f.issue());
        let workers = 8;
        let ready = Arc::new(Barrier::new(workers + 1));
        let finish = Arc::new(Barrier::new(workers + 1));
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let f = Arc::clone(&f);
                let token = Arc::clone(&token);
                let ready = Arc::clone(&ready);
                let finish = Arc::clone(&finish);
                thread::spawn(move || {
                    let registry = f.registry.clone();
                    let pin = registry.get_or_pin(token.expose_secret()).unwrap();
                    ready.wait();
                    finish.wait();
                    assert_eq!(
                        pin.metadata().advertised_models,
                        BTreeSet::from(["updated".into()])
                    );
                    let issued = f.issue();
                    assert!(registry.get_or_pin(issued.expose_secret()).is_ok());
                })
            })
            .collect();
        ready.wait();
        f.advance(IDLE_TIMEOUT.as_secs() * 2);
        f.registry
            .update_advertised(token.expose_secret(), BTreeSet::from(["updated".into()]))
            .unwrap();
        assert_eq!(
            f.registry.0.lock()[&token_digest(token.expose_secret())].pins,
            workers
        );
        finish.wait();
        for handle in handles {
            handle.join().unwrap();
        }
        let entries = f.registry.0.lock();
        assert_eq!(entries.len(), workers + 1);
        assert_eq!(entries[&token_digest(token.expose_secret())].pins, 0);
        drop(entries);
        f.advance(IDLE_TIMEOUT.as_secs());
        assert_eq!(
            f.registry.get_or_pin(token.expose_secret()).unwrap_err(),
            LaunchTokenError::Expired
        );
    }

    #[test]
    fn unwind_releases_pin_and_starts_a_fresh_idle_window() {
        let f = Fixture::new();
        let token = f.issue();
        let pin = f.registry.get_or_pin(token.expose_secret()).unwrap();
        f.advance(IDLE_TIMEOUT.as_secs() * 2);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _pin = pin;
                panic!("simulated request cancellation");
            }))
            .is_err()
        );
        let entries = f.registry.0.lock();
        assert_eq!(entries[&token_digest(token.expose_secret())].pins, 0);
        drop(entries);
        assert!(f.registry.get_or_pin(token.expose_secret()).is_ok());
    }
}
