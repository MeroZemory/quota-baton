//! Process-local observations only: no fetching, credentials, placement or moves.
//! HTTP callers must bound decoded body bytes before constructing a JSON Value.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;

pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const MAX_ACCOUNTS: usize = 64;
pub const MAX_MODELS: usize = 2048;
pub const MAX_RAW_BYTES: usize = 1024 * 1024;
pub const MAX_JSON_NODES: usize = 16_384;
pub const MAX_JSON_DEPTH: usize = 32;
pub const MAX_ID_BYTES: usize = 256;
type Clock = dyn Fn() -> Instant + Send + Sync;

// Content-bearing types deliberately have no derived Debug or serialization.
macro_rules! redacted_debug {
    ($($ty:ty),+ $(,)?) => {$ (
        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($ty), "([REDACTED])"))
            }
        }
    )+};
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("invalid account key")]
    InvalidAccount,
    #[error("catalog account capacity reached")]
    AccountCapacity,
    #[error("catalog refresh already in flight")]
    RefreshInFlight,
    #[error("catalog observation ticket exhausted")]
    TicketExhausted,
    #[error("catalog observation superseded")]
    StaleObservation,
    #[error("invalid or ambiguous catalog shape")]
    InvalidShape,
    #[error("catalog has too many model entries")]
    TooManyModels,
    #[error("invalid model identifier")]
    InvalidModelId,
    #[error("catalog JSON exceeds resource limits")]
    RawLimit,
}

/// Safe classifications only; never store response bodies, URLs or error strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshFailure {
    Transport,
    Unauthorized,
    Upstream,
    Cancelled,
    InvalidCatalog(CatalogError),
}

pub struct KnownCatalog {
    raw: Value,
    supported: BTreeSet<String>,
    observed_at: Instant,
}

impl KnownCatalog {
    /// Preserves all provider capability fields and original array ordering.
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    pub fn supported(&self) -> &BTreeSet<String> {
        &self.supported
    }

    pub fn observed_at(&self) -> Instant {
        self.observed_at
    }
}

#[derive(Clone)]
pub enum Observation {
    Unknown,
    Known(Arc<KnownCatalog>),
    RefreshFailed {
        previous: Option<Arc<KnownCatalog>>,
        reason: RefreshFailure,
    },
}

impl Observation {
    /// A retained catalog is stale evidence, not proof of current entitlement.
    pub fn last_known(&self) -> Option<&Arc<KnownCatalog>> {
        match self {
            Self::Unknown => None,
            Self::Known(known) => Some(known),
            Self::RefreshFailed { previous, .. } => previous.as_ref(),
        }
    }
}

pub struct CatalogView {
    pub observation: Observation,
    pub refresh_due: bool,
    pub refreshing: bool,
}

struct Entry {
    observation: Observation,
    completed_at: Option<Instant>,
    active_ticket: Option<u64>,
}

struct State {
    entries: HashMap<String, Entry>,
    last_ticket: u64,
}

struct Inner {
    state: Mutex<State>,
    clock: Arc<Clock>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

#[derive(Clone)]
pub struct CatalogCache(Arc<Inner>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshMode {
    IfDue,
    Force,
}

impl Default for CatalogCache {
    fn default() -> Self {
        Self::with_clock(Arc::new(Instant::now))
    }
}

impl CatalogCache {
    fn with_clock(clock: Arc<Clock>) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State {
                entries: HashMap::new(),
                last_ticket: 0,
            }),
            clock,
        }))
    }

    /// A missing account is Unknown and due. Reading never inserts or refreshes.
    pub fn view(&self, account: &str) -> Result<CatalogView, CatalogError> {
        validate_account(account)?;
        let state = self.0.lock();
        let now = (self.0.clock)();
        Ok(match state.entries.get(account) {
            Some(entry) => CatalogView {
                observation: entry.observation.clone(),
                refresh_due: due(entry, now),
                refreshing: entry.active_ticket.is_some(),
            },
            None => CatalogView {
                observation: Observation::Unknown,
                refresh_due: true,
                refreshing: false,
            },
        })
    }

    /// None means not due. Even Force never bypasses an active refresh.
    pub fn begin_refresh(
        &self,
        account: &str,
        mode: RefreshMode,
    ) -> Result<Option<RefreshGuard>, CatalogError> {
        validate_account(account)?;
        let mut state = self.0.lock();
        if let Some(entry) = state.entries.get(account) {
            if entry.active_ticket.is_some() {
                return Err(CatalogError::RefreshInFlight);
            }
            if mode == RefreshMode::IfDue && !due(entry, (self.0.clock)()) {
                return Ok(None);
            }
        } else if state.entries.len() >= MAX_ACCOUNTS {
            return Err(CatalogError::AccountCapacity);
        }
        let ticket = state
            .last_ticket
            .checked_add(1)
            .ok_or(CatalogError::TicketExhausted)?;
        state.last_ticket = ticket;
        state
            .entries
            .entry(account.to_owned())
            .or_insert(Entry {
                observation: Observation::Unknown,
                completed_at: None,
                active_ticket: None,
            })
            .active_ticket = Some(ticket);
        Ok(Some(RefreshGuard {
            inner: Arc::clone(&self.0),
            account: account.to_owned(),
            ticket,
            finished: false,
        }))
    }

    /// Explicit lifecycle cleanup only. Never remove an in-flight account.
    pub fn remove(&self, account: &str) -> Result<bool, CatalogError> {
        validate_account(account)?;
        let mut state = self.0.lock();
        if state
            .entries
            .get(account)
            .is_some_and(|e| e.active_ticket.is_some())
        {
            return Err(CatalogError::RefreshInFlight);
        }
        Ok(state.entries.remove(account).is_some())
    }
}

fn validate_account(account: &str) -> Result<(), CatalogError> {
    if account.is_empty() || account.len() > MAX_ID_BYTES || account.chars().any(char::is_control) {
        return Err(CatalogError::InvalidAccount);
    }
    Ok(())
}

fn due(entry: &Entry, now: Instant) -> bool {
    entry
        .completed_at
        .is_none_or(|last| now.saturating_duration_since(last) >= REFRESH_INTERVAL)
}

/// Move into the HTTP future; cancellation/unwind releases admission on Drop.
#[must_use = "retain until fetch completes or is cancelled"]
pub struct RefreshGuard {
    inner: Arc<Inner>,
    account: String,
    ticket: u64,
    finished: bool,
}

impl RefreshGuard {
    /// Validate outside the mutex. Failure preserves the previous known catalog.
    /// Upstream failure is recorded successfully (Ok); invalid JSON returns Err
    /// as well as recording InvalidCatalog. A stale completion changes nothing.
    pub fn finish(mut self, reply: Result<Value, RefreshFailure>) -> Result<(), CatalogError> {
        let parsed = reply.map(parse_catalog);
        let validation_error = match &parsed {
            Ok(Err(error)) => Some(*error),
            _ => None,
        };
        let mut state = self.inner.lock();
        let Some(entry) = state.entries.get_mut(&self.account) else {
            self.finished = true;
            return Err(CatalogError::StaleObservation);
        };
        if entry.active_ticket != Some(self.ticket) {
            self.finished = true;
            return Err(CatalogError::StaleObservation);
        }
        let now = (self.inner.clock)();
        entry.observation = match parsed {
            Ok(Ok((raw, supported))) => Observation::Known(Arc::new(KnownCatalog {
                raw,
                supported,
                observed_at: now,
            })),
            outcome => Observation::RefreshFailed {
                previous: entry.observation.last_known().cloned(),
                reason: match outcome {
                    Ok(Err(error)) => RefreshFailure::InvalidCatalog(error),
                    Err(reason) => reason,
                    Ok(Ok(_)) => unreachable!(),
                },
            },
        };
        entry.completed_at = Some(now);
        entry.active_ticket = None;
        self.finished = true;
        validation_error.map_or(Ok(()), Err)
    }
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = self.inner.lock();
        if let Some(entry) = state.entries.get_mut(&self.account)
            && entry.active_ticket == Some(self.ticket)
        {
            entry.observation = Observation::RefreshFailed {
                previous: entry.observation.last_known().cloned(),
                reason: RefreshFailure::Cancelled,
            };
            entry.completed_at = Some((self.inner.clock)());
            entry.active_ticket = None;
        }
    }
}

redacted_debug!(
    KnownCatalog,
    Observation,
    CatalogView,
    CatalogCache,
    RefreshGuard
);

fn parse_catalog(raw: Value) -> Result<(Value, BTreeSet<String>), CatalogError> {
    validate_raw(&raw, 0, &mut 0, &mut 0)?;
    serde_json::to_writer(ByteBudget(MAX_RAW_BYTES), &raw).map_err(|_| CatalogError::RawLimit)?;
    let object = raw.as_object().ok_or(CatalogError::InvalidShape)?;
    let (items, field) = match (object.get("models"), object.get("data")) {
        (Some(items), None) => (items, "slug"),
        (None, Some(items)) => (items, "id"),
        _ => return Err(CatalogError::InvalidShape),
    };
    let items = items.as_array().ok_or(CatalogError::InvalidShape)?;
    if items.len() > MAX_MODELS {
        return Err(CatalogError::TooManyModels);
    }
    let mut supported = BTreeSet::new();
    for item in items {
        let id = item
            .as_object()
            .and_then(|item| item.get(field))
            .and_then(Value::as_str)
            .ok_or(CatalogError::InvalidModelId)?;
        if id.is_empty()
            || id.len() > MAX_ID_BYTES
            || id.chars().any(char::is_whitespace)
            || id.chars().any(char::is_control)
        {
            return Err(CatalogError::InvalidModelId);
        }
        supported.insert(id.to_owned());
    }
    Ok((raw, supported))
}

// Count structure and unescaped string/key bytes before bounded serialization.
// Recursion is capped before descending, and no mirror JSON allocation is made.
fn validate_raw(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
    strings: &mut usize,
) -> Result<(), CatalogError> {
    *nodes += 1;
    if depth > MAX_JSON_DEPTH || *nodes > MAX_JSON_NODES {
        return Err(CatalogError::RawLimit);
    }
    let mut add_bytes = |bytes: usize| -> Result<(), CatalogError> {
        *strings = strings.checked_add(bytes).ok_or(CatalogError::RawLimit)?;
        if *strings > MAX_RAW_BYTES {
            return Err(CatalogError::RawLimit);
        }
        Ok(())
    };
    match value {
        Value::String(text) => add_bytes(text.len())?,
        Value::Array(items) => {
            if items.len() > MAX_JSON_NODES - *nodes {
                return Err(CatalogError::RawLimit);
            }
            for item in items {
                validate_raw(item, depth + 1, nodes, strings)?;
            }
        }
        Value::Object(object) => {
            if object.len() > MAX_JSON_NODES - *nodes {
                return Err(CatalogError::RawLimit);
            }
            // End the mutable closure borrow before recursion below.
            for key in object.keys() {
                add_bytes(key.len())?;
            }
            for item in object.values() {
                validate_raw(item, depth + 1, nodes, strings)?;
            }
        }
        _ => {}
    }
    Ok(())
}

struct ByteBudget(usize);
impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(io::Error::other("catalog size limit"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    struct Fixture {
        cache: CatalogCache,
        seconds: Arc<AtomicU64>,
    }
    impl Fixture {
        fn new() -> Self {
            let origin = Instant::now();
            let seconds = Arc::new(AtomicU64::new(0));
            let clock_seconds = Arc::clone(&seconds);
            Self {
                cache: CatalogCache::with_clock(Arc::new(move || {
                    origin + Duration::from_secs(clock_seconds.load(Ordering::SeqCst))
                })),
                seconds,
            }
        }
        fn advance(&self, seconds: u64) {
            self.seconds.fetch_add(seconds, Ordering::SeqCst);
        }
        fn begin(&self, account: &str) -> RefreshGuard {
            self.cache
                .begin_refresh(account, RefreshMode::Force)
                .unwrap()
                .unwrap()
        }
    }

    #[test]
    fn reads_are_unknown_and_never_insert() {
        let f = Fixture::new();
        let view = f.cache.view("a").unwrap();
        assert!(matches!(view.observation, Observation::Unknown));
        assert!(view.refresh_due && !view.refreshing);
        assert!(f.cache.0.lock().entries.is_empty());
    }

    #[test]
    fn both_formats_preserve_raw_fields_and_deduplicate_ids() {
        let f = Fixture::new();
        for raw in [
            json!({"models":[{"slug":"a", "capability":{"future":true}}, {"slug":"a"}], "extra":7}),
            json!({"data":[{"id":"a", "owned_by":"provider"}], "object":"list"}),
        ] {
            f.begin("a").finish(Ok(raw.clone())).unwrap();
            let view = f.cache.view("a").unwrap();
            let known = view.observation.last_known().unwrap();
            assert_eq!(known.raw(), &raw);
            assert_eq!(known.supported(), &BTreeSet::from(["a".to_owned()]));
        }
    }

    #[test]
    fn empty_known_differs_from_unknown_and_failed_without_previous() {
        let f = Fixture::new();
        f.begin("empty").finish(Ok(json!({"models":[]}))).unwrap();
        assert!(matches!(
            f.cache.view("empty").unwrap().observation,
            Observation::Known(_)
        ));
        f.begin("failed")
            .finish(Err(RefreshFailure::Unauthorized))
            .unwrap();
        assert!(matches!(
            f.cache.view("failed").unwrap().observation,
            Observation::RefreshFailed {
                previous: None,
                reason: RefreshFailure::Unauthorized
            }
        ));
    }

    #[test]
    fn failures_and_cancellation_preserve_last_success_and_observed_time() {
        let f = Fixture::new();
        f.begin("a")
            .finish(Ok(json!({"data":[{"id":"old"}]})))
            .unwrap();
        let original = f
            .cache
            .view("a")
            .unwrap()
            .observation
            .last_known()
            .unwrap()
            .clone();
        f.advance(10);
        f.begin("a").finish(Err(RefreshFailure::Transport)).unwrap();
        drop(f.begin("a"));
        let view = f.cache.view("a").unwrap();
        assert!(matches!(
            view.observation,
            Observation::RefreshFailed {
                reason: RefreshFailure::Cancelled,
                ..
            }
        ));
        let previous = view.observation.last_known().unwrap();
        assert!(Arc::ptr_eq(previous, &original));
        assert_eq!(previous.observed_at(), original.observed_at());
        assert!(!view.refreshing);
    }

    #[test]
    fn due_is_exactly_five_minutes_after_completion_including_failure() {
        let f = Fixture::new();
        let guard = f.begin("a");
        f.advance(900);
        assert!(f.cache.view("a").unwrap().refreshing);
        guard.finish(Err(RefreshFailure::Upstream)).unwrap();
        f.advance(299);
        assert!(!f.cache.view("a").unwrap().refresh_due);
        assert!(
            f.cache
                .begin_refresh("a", RefreshMode::IfDue)
                .unwrap()
                .is_none()
        );
        f.advance(1);
        assert!(f.cache.view("a").unwrap().refresh_due);
        assert!(
            f.cache
                .begin_refresh("a", RefreshMode::IfDue)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn singleflight_and_cancel_release_are_per_account() {
        let f = Fixture::new();
        let guard = f.begin("a");
        assert_eq!(
            f.cache.begin_refresh("a", RefreshMode::Force).unwrap_err(),
            CatalogError::RefreshInFlight
        );
        let other = f.begin("b");
        assert_eq!(f.cache.remove("a"), Err(CatalogError::RefreshInFlight));
        drop(guard);
        assert!(
            f.cache
                .begin_refresh("a", RefreshMode::Force)
                .unwrap()
                .is_some()
        );
        drop(other);
    }

    #[test]
    fn old_ticket_cannot_publish_or_cancel_new_refresh() {
        let f = Fixture::new();
        let old = f.begin("a");
        // Simulate a delayed duplicate completion; public guards cannot be cloned.
        let late = RefreshGuard {
            inner: Arc::clone(&old.inner),
            account: old.account.clone(),
            ticket: old.ticket,
            finished: false,
        };
        let late_drop = RefreshGuard {
            inner: Arc::clone(&old.inner),
            account: old.account.clone(),
            ticket: old.ticket,
            finished: false,
        };
        let late_after = RefreshGuard {
            inner: Arc::clone(&old.inner),
            account: old.account.clone(),
            ticket: old.ticket,
            finished: false,
        };
        old.finish(Ok(json!({"models":[]}))).unwrap();
        assert!(f.cache.remove("a").unwrap());
        let current = f.begin("a");
        assert!(current.ticket > late.ticket);
        drop(late_drop);
        assert!(f.cache.view("a").unwrap().refreshing);
        assert_eq!(
            late.finish(Ok(json!({"data":[{"id":"stale"}]}))),
            Err(CatalogError::StaleObservation)
        );
        assert!(f.cache.view("a").unwrap().refreshing);
        current.finish(Ok(json!({"data":[{"id":"new"}]}))).unwrap();
        assert_eq!(
            late_after.finish(Ok(json!({"data":[{"id":"stale"}]}))),
            Err(CatalogError::StaleObservation)
        );
        assert!(
            f.cache
                .view("a")
                .unwrap()
                .observation
                .last_known()
                .unwrap()
                .supported()
                .contains("new")
        );
    }

    #[test]
    fn success_and_cancellation_schedule_from_completion_and_force_bypasses_due() {
        let f = Fixture::new();
        f.begin("a").finish(Ok(json!({"models":[]}))).unwrap();
        f.advance(REFRESH_INTERVAL.as_secs() - 1);
        assert!(!f.cache.view("a").unwrap().refresh_due);
        f.advance(1);
        assert!(f.cache.view("a").unwrap().refresh_due);
        drop(f.begin("a"));
        assert!(!f.cache.view("a").unwrap().refresh_due);
        let guard = f.begin("a");
        f.advance(REFRESH_INTERVAL.as_secs() * 2);
        guard.finish(Ok(json!({"models":[]}))).unwrap();
        assert!(!f.cache.view("a").unwrap().refresh_due);
    }

    #[test]
    fn separate_accounts_do_not_share_catalogs_or_failure_state() {
        let f = Fixture::new();
        f.begin("a")
            .finish(Ok(json!({"data":[{"id":"only-a"}]})))
            .unwrap();
        f.begin("b")
            .finish(Ok(json!({"data":[{"id":"only-b"}]})))
            .unwrap();
        f.begin("a")
            .finish(Err(RefreshFailure::Unauthorized))
            .unwrap();
        let a = f.cache.view("a").unwrap();
        let b = f.cache.view("b").unwrap();
        assert!(matches!(a.observation, Observation::RefreshFailed { .. }));
        assert!(matches!(b.observation, Observation::Known(_)));
        assert!(
            a.observation
                .last_known()
                .unwrap()
                .supported()
                .contains("only-a")
        );
        assert!(
            !a.observation
                .last_known()
                .unwrap()
                .supported()
                .contains("only-b")
        );
        assert!(
            b.observation
                .last_known()
                .unwrap()
                .supported()
                .contains("only-b")
        );
    }

    #[test]
    fn invalid_catalog_preserves_previous_and_returns_explicit_error() {
        let f = Fixture::new();
        f.begin("a").finish(Ok(json!({"models":[]}))).unwrap();
        for raw in [
            json!(null),
            json!({}),
            json!({"models":[], "data":[]}),
            json!({"models":{}}),
            json!({"models":[{}]}),
            json!({"data":[{"id":""}]}),
            json!({"data":[{"id":"bad\nvalue"}]}),
        ] {
            assert!(f.begin("a").finish(Ok(raw)).is_err());
            assert!(matches!(
                f.cache.view("a").unwrap().observation,
                Observation::RefreshFailed {
                    previous: Some(_),
                    reason: RefreshFailure::InvalidCatalog(_)
                }
            ));
        }
    }

    #[test]
    fn raw_and_model_resource_limits_are_explicit() {
        assert_eq!(
            parse_catalog(json!({"models":vec![json!({"slug":"x"}); MAX_MODELS + 1]})).unwrap_err(),
            CatalogError::TooManyModels
        );
        assert_eq!(
            parse_catalog(json!({"models":[], "extra":"x".repeat(MAX_RAW_BYTES + 1)})).unwrap_err(),
            CatalogError::RawLimit
        );
        // Escaping pushes serialized size over the cap despite fewer input bytes.
        assert_eq!(
            parse_catalog(json!({"models":[], "extra":"\n".repeat(MAX_RAW_BYTES / 2)}))
                .unwrap_err(),
            CatalogError::RawLimit
        );
        assert_eq!(
            parse_catalog(json!({"models":[], "extra":vec![Value::Null; MAX_JSON_NODES]}))
                .unwrap_err(),
            CatalogError::RawLimit
        );
        let mut nested = Value::Null;
        for _ in 0..MAX_JSON_DEPTH + 1 {
            nested = Value::Array(vec![nested]);
        }
        assert_eq!(
            parse_catalog(json!({"models":[], "extra":nested})).unwrap_err(),
            CatalogError::RawLimit
        );
        assert_eq!(
            parse_catalog(json!({"data":[{"id":"x".repeat(MAX_ID_BYTES + 1)}]})).unwrap_err(),
            CatalogError::InvalidModelId
        );
    }

    #[test]
    fn capacity_has_explicit_cleanup_and_tickets_never_wrap() {
        let f = Fixture::new();
        for i in 0..MAX_ACCOUNTS {
            drop(f.begin(&format!("account-{i}")));
        }
        assert_eq!(
            f.cache
                .begin_refresh("overflow", RefreshMode::Force)
                .unwrap_err(),
            CatalogError::AccountCapacity
        );
        assert!(f.cache.remove("account-0").unwrap());
        drop(f.begin("replacement"));
        assert_eq!(f.cache.0.lock().entries.len(), MAX_ACCOUNTS);
        f.cache.0.lock().last_ticket = u64::MAX;
        assert_eq!(
            f.cache
                .begin_refresh("replacement", RefreshMode::Force)
                .unwrap_err(),
            CatalogError::TicketExhausted
        );
        assert!(!f.cache.view("replacement").unwrap().refreshing);
    }

    #[test]
    fn cancellation_on_unwind_and_registry_lifetime() {
        let f = Fixture::new();
        let guard = f.begin("a");
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _guard = guard;
                panic!("simulated cancellation");
            }))
            .is_err()
        );
        assert!(!f.cache.view("a").unwrap().refreshing);
        let guard = f.begin("a");
        drop(f);
        guard.finish(Ok(json!({"models":[]}))).unwrap();
    }

    #[test]
    fn concurrent_admission_has_one_winner() {
        let f = Fixture::new();
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let cache = f.cache.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let result = cache.begin_refresh("a", RefreshMode::Force);
                    barrier.wait(); // Winner retains admission until both attempts finish.
                    result
                })
            })
            .collect();
        barrier.wait();
        barrier.wait();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| matches!(r, Ok(Some(_)))).count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(CatalogError::RefreshInFlight)))
                .count(),
            1
        );
    }

    #[test]
    fn debug_redacts_all_content_and_account_keys_are_validated() {
        let f = Fixture::new();
        let guard = f.begin("private-account");
        let guard_debug = format!("{guard:?}");
        guard
            .finish(Ok(
                json!({"data":[{"id":"private-model"}], "future":"private-value"}),
            ))
            .unwrap();
        let view = f.cache.view("private-account").unwrap();
        let debug = format!(
            "{:?} {view:?} {:?} {:?} {guard_debug}",
            f.cache,
            view.observation,
            view.observation.last_known().unwrap()
        );
        for secret in ["private-account", "private-model", "private-value"] {
            assert!(!debug.contains(secret));
        }
        for invalid in [
            String::new(),
            "x".repeat(MAX_ID_BYTES + 1),
            "bad\nkey".to_owned(),
        ] {
            assert_eq!(
                f.cache.view(&invalid).unwrap_err(),
                CatalogError::InvalidAccount
            );
            assert_eq!(
                f.cache
                    .begin_refresh(&invalid, RefreshMode::Force)
                    .unwrap_err(),
                CatalogError::InvalidAccount
            );
        }
    }
}
