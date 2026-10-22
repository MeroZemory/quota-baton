//! Minimum metadata schema and bounded plain-JSONL storage. Caller owns runtime hooks.
//! Initial witnesses are not snapshots: this version NEVER asserts replay completeness.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

// Distinct from the preserved larger schema-2 tapes; never reinterpret their layout.
const SCHEMA: u16 = 3;
const ID_DOMAIN_VERSION: u16 = 1;
const MAX_RECORD: usize = 8192;
const QUEUE_RECORDS: usize = 1024; // <=8 MiB encoded payload, plus bounded container overhead.
const SEGMENT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SEGMENTS: usize = 256;
const DISK_BYTES: u64 = 256 * 1024 * 1024;
const MANIFEST_RESERVE: u64 = 512 * 1024;
const MAX_EPOCHS: usize = 8;
const CONTROL_BYTES: u64 = 8192;
const STORE_RESERVE: u64 =
    MAX_EPOCHS as u64 * (MANIFEST_RESERVE + CONTROL_BYTES) + 2 * CONTROL_BYTES;
const HOUR_NS: u64 = 3_600_000_000_000;
const RETENTION_NS: u64 = 7 * 24 * HOUR_NS;

/// Validated logical-byte budget for ONE dedicated root, across all epochs.
/// Main maps operator config into this value; no config/file lookup occurs here.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    disk_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            disk_bytes: DISK_BYTES,
        }
    }
}
impl Limits {
    pub fn new(disk_bytes: u64) -> io::Result<Self> {
        if !(16 * 1024 * 1024..=512 * 1024 * 1024).contains(&disk_bytes) {
            return Err(io::Error::other("capture_disk_limit"));
        }
        Ok(Self { disk_bytes })
    }
    pub fn disk_bytes(self) -> u64 {
        self.disk_bytes
    }
}

/// Constant-size, explicitly partial coverage; no initial/hook receipt is implied.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct KindCounters {
    pub attempted: u64,
    pub queued: u64,
    pub persisted: u64,
    pub rejected: u64,
    pub unpersisted: u64,
    pub encoded_bytes: u64,
    pub persisted_bytes: u64,
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
    pub first_persisted_sequence: Option<u64>,
    pub last_persisted_sequence: Option<u64>,
    pub saturated: bool,
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct OwnerCoverage {
    pub first_cursor: Option<u64>,
    pub last_cursor: Option<u64>,
    pub cursor_gaps: u64,
    pub rejected: u64,
    pub saturated: bool,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[repr(usize)]
pub enum EventKind {
    Ingress,
    Account,
    Quota,
    CollectionMember,
    Decision,
    Handle,
    Usage,
    Operator,
    Alias,
    InitialWitness,
    Unclassified,
}
const KIND_COUNT: usize = 11;
const EVENT_KINDS: [EventKind; KIND_COUNT] = [
    EventKind::Ingress,
    EventKind::Account,
    EventKind::Quota,
    EventKind::CollectionMember,
    EventKind::Decision,
    EventKind::Handle,
    EventKind::Usage,
    EventKind::Operator,
    EventKind::Alias,
    EventKind::InitialWitness,
    EventKind::Unclassified,
];
impl Event {
    // Exhaustive: every retained fixed DTO has an explicit coverage classification.
    fn kind(&self) -> EventKind {
        match self {
            Self::Ingress { .. } => EventKind::Ingress,
            Self::Account { .. } | Self::AccountState { .. } => EventKind::Account,
            Self::Quota { .. } => EventKind::Quota,
            Self::CollectionMember { .. } | Self::CollectionBoundary { .. } => {
                EventKind::CollectionMember
            }
            Self::Decision { .. } => EventKind::Decision,
            Self::Handle { .. } => EventKind::Handle,
            Self::Usage { .. } => EventKind::Usage,
            Self::Operator { .. } => EventKind::Operator,
            Self::Alias { .. } => EventKind::Alias,
            Self::InitialWitness { .. } => EventKind::InitialWitness,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreStatus {
    expiry_removal_attempts: u64,
    expired_epoch_loss_unknown: bool,
    last_utc_ns: Option<i64>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EpochHeader {
    schema: u16,
    start_utc_ns: i64,
}
/// Lock is held by the sole Archive until close/drop, and released by OS on crash.
/// Eight generated slots only; unknown files/symlinks cause a closed failure.
struct EpochStore {
    root: PathBuf,
    _lock: fs::File,
    limits: Limits,
    slot: usize,
    other_bytes: u64,
    status: StoreStatus,
}
fn read_control<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    use std::io::Read;
    let file = fs::File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > CONTROL_BYTES {
        return Err(io::Error::other("capture_control_size"));
    }
    let mut bytes = Vec::new();
    file.take(CONTROL_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > CONTROL_BYTES {
        return Err(io::Error::other("capture_control_size"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
fn write_control(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() as u64 > CONTROL_BYTES {
        return Err(io::Error::other("capture_control_size"));
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()
}
fn regular_file(path: &Path) -> io::Result<u64> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(io::Error::other("capture_foreign_entry"));
    }
    Ok(meta.len())
}
impl EpochStore {
    fn epoch_path(&self, slot: usize) -> PathBuf {
        self.root.join(format!("e{slot:02}"))
    }
    fn write_status(&self) -> io::Result<()> {
        let pending = self.root.join("store.pending");
        // Only generated control files are replaced, under the OS-held root lock.
        match fs::symlink_metadata(&pending) {
            Ok(_) => {
                regular_file(&pending)?;
                fs::remove_file(&pending)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        write_control(&pending, &self.status)?;
        fs::rename(pending, self.root.join("store.json"))
    }
    fn scan(&self) -> io::Result<Vec<(usize, EpochHeader, u64)>> {
        let mut epochs = Vec::new();
        for (count, entry) in fs::read_dir(&self.root)?.enumerate() {
            if count >= MAX_EPOCHS + 3 {
                return Err(io::Error::other("capture_foreign_entry"));
            }
            let entry = entry?;
            let name = entry.file_name();
            if [".lock", "store.json", "store.pending"]
                .iter()
                .any(|n| name == *n)
            {
                let len = regular_file(&entry.path())?;
                if name == ".lock" && len != 0 || name != ".lock" && len > CONTROL_BYTES {
                    return Err(io::Error::other("capture_control_size"));
                }
                continue;
            }
            let Some(slot) = (0..MAX_EPOCHS).find(|slot| name == format!("e{slot:02}").as_str())
            else {
                return Err(io::Error::other("capture_foreign_entry"));
            };
            if !fs::symlink_metadata(entry.path())?.file_type().is_dir() {
                return Err(io::Error::other("capture_foreign_entry"));
            }
            let mut bytes = 0u64;
            for (n, file) in fs::read_dir(entry.path())?.enumerate() {
                if n >= MAX_SEGMENTS + 3 {
                    return Err(io::Error::other("capture_file_count"));
                }
                let file = file?;
                let name = file.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| io::Error::other("capture_foreign_entry"))?;
                let segment = name.len() == 23
                    && name.starts_with('s')
                    && name.ends_with(".jsonl")
                    && name.as_bytes()[1..17].iter().all(u8::is_ascii_hexdigit);
                if !segment && !["epoch.json", "manifest.json", "manifest.pending"].contains(&name)
                {
                    return Err(io::Error::other("capture_foreign_entry"));
                }
                let size = regular_file(&file.path())?;
                let max = if segment {
                    SEGMENT_BYTES
                } else if name == "epoch.json" {
                    CONTROL_BYTES
                } else {
                    MANIFEST_RESERVE / 2
                };
                if size > max {
                    return Err(io::Error::other("capture_file_size"));
                }
                bytes = bytes
                    .checked_add(size)
                    .ok_or_else(|| io::Error::other("capture_size_overflow"))?;
            }
            let header: EpochHeader = read_control(&entry.path().join("epoch.json"))?;
            if header.schema != SCHEMA {
                return Err(io::Error::other("capture_epoch_schema"));
            }
            epochs.push((slot, header, bytes));
        }
        Ok(epochs)
    }
    fn maintain(&mut self, now: Stamp) -> io::Result<()> {
        if self
            .status
            .last_utc_ns
            .is_some_and(|prior| now.utc_ns < prior)
        {
            return Err(io::Error::other("capture_restart_clock_regression"));
        }
        let epochs = self.scan()?;
        for (slot, header, _) in &epochs {
            if *slot == self.slot
                || now.utc_ns.saturating_sub(header.start_utc_ns) < RETENTION_NS as i64
            {
                continue;
            }
            // Write the loss witness BEFORE deleting; a crash may overcount attempts,
            // but can never silently certify the removed epoch as complete coverage.
            self.status.expiry_removal_attempts = self
                .status
                .expiry_removal_attempts
                .checked_add(1)
                .ok_or_else(|| io::Error::other("capture_counter_exhausted"))?;
            self.status.expired_epoch_loss_unknown = true;
            self.write_status()?;
            let path = self.epoch_path(*slot);
            for entry in fs::read_dir(&path)? {
                fs::remove_file(entry?.path())?;
            }
            fs::remove_dir(path)?;
        }
        self.other_bytes = self
            .scan()?
            .into_iter()
            .filter(|(slot, _, _)| *slot != self.slot)
            .try_fold(0u64, |sum, (_, _, size)| {
                sum.checked_add(size)
                    .ok_or_else(|| io::Error::other("capture_size_overflow"))
            })?;
        if self
            .other_bytes
            .checked_add(STORE_RESERVE)
            .is_none_or(|n| n > self.limits.disk_bytes)
        {
            return Err(io::Error::other("capture_root_capacity"));
        }
        self.status.last_utc_ns = Some(now.utc_ns);
        self.write_status()
    }
    fn open(root: &Path, limits: Limits, at: Stamp) -> io::Result<(Self, PathBuf)> {
        match fs::create_dir(root) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        if !fs::symlink_metadata(root)?.file_type().is_dir() {
            return Err(io::Error::other("capture_root_type"));
        }
        let lock_path = root.join(".lock");
        match fs::symlink_metadata(&lock_path) {
            Ok(_) => {
                regular_file(&lock_path)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        fs2::FileExt::try_lock_exclusive(&lock)?;
        let status = match fs::symlink_metadata(root.join("store.json")) {
            Ok(_) => {
                regular_file(&root.join("store.json"))?;
                read_control(&root.join("store.json"))?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => StoreStatus::default(),
            Err(e) => return Err(e),
        };
        let mut store = Self {
            root: root.to_owned(),
            _lock: lock,
            limits,
            slot: MAX_EPOCHS,
            other_bytes: 0,
            status,
        };
        let inventory = store.scan()?;
        if !inventory.is_empty() && store.status.last_utc_ns.is_none() {
            return Err(io::Error::other("capture_missing_store_state"));
        }
        store.maintain(at)?;
        let inventory = store.scan()?;
        store.slot = (0..MAX_EPOCHS)
            .find(|slot| !inventory.iter().any(|(used, _, _)| used == slot))
            .ok_or_else(|| io::Error::other("capture_epoch_slots_full"))?;
        let path = store.epoch_path(store.slot);
        fs::create_dir(&path)?;
        write_control(
            &path.join("epoch.json"),
            &EpochHeader {
                schema: SCHEMA,
                start_utc_ns: at.utc_ns,
            },
        )?;
        Ok((store, path))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Id([u8; 32]);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Fingerprint(pub [u8; 32]);
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum IdentityDomain {
    Session,
    Account,
    Model,
    Policy,
    Operation,
    Incident,
    Anchor,
    Pool,
    Window,
}

// HMAC-SHA256, RFC 2104. The key never enters a DTO, debug output or export.
fn pseudonym(key: &[u8; 32], domain: IdentityDomain, raw: &[u8]) -> Id {
    let mut inner = [0x36; 64];
    let mut outer = [0x5c; 64];
    for i in 0..32 {
        inner[i] ^= key[i];
        outer[i] ^= key[i];
    }
    let mut hash = Sha256::new();
    hash.update(inner);
    hash.update(ID_DOMAIN_VERSION.to_be_bytes());
    hash.update([domain as u8]);
    hash.update((raw.len() as u64).to_be_bytes());
    hash.update(raw);
    let mut result = Sha256::new();
    result.update(outer);
    result.update(hash.finalize());
    Id(result.finalize().into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Stamp {
    pub utc_ns: i64,
    pub monotonic_ns: u64,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Provider {
    Codex,
    Claude,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Transport {
    Http,
    WebSocket,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Route {
    Responses,
    Messages,
    Launch,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[repr(u8)]
pub enum Owner {
    Relay,
    Collector,
    Runtime,
    Bindings,
    Policy,
    Capacity,
    Accounting,
    Control,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Origin {
    IndependentPoll,
    Operator,
    ResponseAttempt { attempt: u64 },
    Probe { attempt: u64 },
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Window {
    FiveHour,
    SevenDay,
    Session,
    OtherSupported,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum CollectionSide {
    Source,
    Target,
    SourceAndTarget,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Exclusion {
    None,
    Tried,
    Reconnect,
    Capacity,
    Health,
    Entitlement,
    Unknown,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Disposition {
    Dispatch,
    Hold,
    Refuse,
    NoEligible,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Guard {
    Allowed,
    ReturnVeto,
    SessionLimit,
    GlobalIncidentLimit,
    ActiveTrip,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Lifecycle {
    Reserved,
    Sent,
    Released,
    Cancelled,
    Expired,
    Pin,
    Unpin,
    SocketOpen,
    SocketClose,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Terminal {
    Success,
    Error,
    Incomplete,
    Cancelled,
    Unknown,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub enum Intent {
    Alias,
    GuardClear,
    Pause,
    Drain,
    Remove,
    PolicyEdit,
    Recover,
}

/// Fixed allowlist only; no string, Value, Account, Candidate, path or body field.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(tag = "kind", content = "facts")]
pub enum Event {
    CollectionBoundary {
        collection: u64,
        started: Option<Stamp>,
        rows: u32,
        complete: bool,
    },
    AccountState {
        facts: super::replay_projection::AccountFacts,
    },
    Ingress {
        request: u64,
        session: Id,
        parent: Option<Id>,
        provider: Provider,
        transport: Transport,
        route: Route,
        routing_model: Option<Id>,
        accounting_model: Option<Id>,
        request_cost_bound: Option<u64>,
        deadline_mono_ns: u64,
    },
    Account {
        account: Id,
        provider: Provider,
        order_rank: u32,
        revision: u64,
        eligible: Option<bool>,
        credential_available: bool,
        drain: bool,
        origin: Origin,
    },
    Quota {
        account: Id,
        window: Window,
        revision: u64,
        observed: Stamp,
        finite_percent_bits: Option<u64>,
        reset_utc_ns: Option<i64>,
        origin: Origin,
    },
    CollectionMember {
        collection: u64,
        account: Id,
        side: CollectionSide,
        started: Stamp,
        fact_revision: Option<u64>,
        order_rank: u32,
        exclusion: Exclusion,
    },
    Decision {
        request: u64,
        canonical_session: Id,
        epoch: u64,
        source: Option<Id>,
        assigned: Option<Id>,
        disposition: Disposition,
        source_collection: Option<u64>,
        target_collection: u64,
        policy_revision: u64,
        guard: Guard,
        incident: Option<Id>,
    },
    Handle {
        request: u64,
        attempt: u64,
        handle: u64,
        account: Id,
        action: Lifecycle,
    },
    Usage {
        attempt: u64,
        account: Id,
        model: Option<Id>,
        terminal: Terminal,
        input: Option<u64>,
        cached: Option<u64>,
        cache_creation: Option<u64>,
        cache_read: Option<u64>,
    },
    Operator {
        operation: Id,
        intent: Intent,
        scope: Option<Id>,
        revision: u64,
    },
    Alias {
        operation: Id,
        from: Id,
        to: Id,
        account: Id,
        committed: bool,
    },
    // A digest/revision witness is NOT the underlying owner state or a reconciled cut.
    InitialWitness {
        component: Owner,
        revision: u64,
        cursor: u64,
        rows: u32,
        digest: Fingerprint,
    },
}

impl Event {
    fn valid(&self) -> bool {
        match self {
            Self::Quota {
                finite_percent_bits: Some(bits),
                ..
            } => f64::from_bits(*bits).is_finite(),
            Self::Decision {
                assigned,
                disposition,
                ..
            } => assigned.is_some() == matches!(disposition, Disposition::Dispatch),
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub enum Provenance {
    Synthetic,
    OperatorOptIn,
}
// Unknown metadata is None, never a sentinel digest. An executable digest alone
// does not attest its source revision, build inputs, or replay adapter identity.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct BuildIdentity {
    pub installed_executable: Option<Fingerprint>,
    pub installed_source: Option<Fingerprint>,
    pub candidate_source: Option<Fingerprint>,
    pub installed_build: Option<Fingerprint>,
    pub candidate_build: Option<Fingerprint>,
    pub config: Option<Fingerprint>,
    pub toolchain: Option<Fingerprint>,
    pub dependencies: Option<Fingerprint>,
    pub installed_adapter: Option<Fingerprint>,
    pub candidate_adapter: Option<Fingerprint>,
    pub projection: Option<Fingerprint>,
    pub recorder: Option<Fingerprint>,
}
#[derive(Serialize)]
struct Envelope {
    schema: u16,
    capture: Id,
    enqueue_sequence: u64,
    at: Stamp,
    owner: Owner,
    owner_cursor: u64,
    cause_enqueue_sequence: Option<u64>,
    event: Event,
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Loss {
    pub unsupported_kind: u64,
    pub queue_full: u64,
    pub invalid: u64,
    pub sequence_exhausted: u64,
    pub clock_regressed: u64,
    pub owner_gap: u64,
    pub producer_failure: u64,
    pub oversized: u64,
    pub disk_cap: u64,
    pub write_error: u64,
    pub unpersisted: u64,
    pub retention_boundary_segments: u64,
    pub counters_saturated: bool,
}
fn increment(value: &mut u64, saturated: &mut bool, by: u64) {
    match value.checked_add(by) {
        Some(next) => *value = next,
        None => {
            *value = u64::MAX;
            *saturated = true;
        }
    }
}
struct Pending {
    kind: EventKind,
    sequence: u64,
    at: Stamp,
    line: Vec<u8>,
}
struct State {
    enabled: bool,
    next_sequence: u64,
    queue: VecDeque<Pending>,
    start: Stamp,
    last: Stamp,
    cursors: [Option<u64>; 8],
    loss: Loss,
    kinds: [KindCounters; KIND_COUNT],
    coverage: [OwnerCoverage; 8],
}
impl State {
    fn reject(&mut self, kind: EventKind, owner: Owner) {
        let counters = &mut self.kinds[kind as usize];
        increment(&mut counters.rejected, &mut counters.saturated, 1);
        let coverage = &mut self.coverage[owner as usize];
        increment(&mut coverage.rejected, &mut coverage.saturated, 1);
    }
    fn unpersisted(&mut self, pending: &Pending) {
        let counters = &mut self.kinds[pending.kind as usize];
        increment(&mut counters.unpersisted, &mut counters.saturated, 1);
    }
}
struct Shared {
    capture: Id,
    key: [u8; 32],
    build: BuildIdentity,
    provenance: Provenance,
    state: Mutex<State>,
}

/// None is the default: no key, filesystem work, thread, sampling or capture.
#[derive(Clone, Default)]
pub struct Recorder {
    shared: Option<Arc<Shared>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordResult {
    Disabled,
    Queued(u64),
    Rejected,
}
// Serde implements fixed-array Serialize only through length 32; the wire
// shape remains the same ordered sequence as additional kinds are appended.
fn serialize_kinds<S: serde::Serializer>(
    kinds: &[(EventKind, KindCounters); KIND_COUNT],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    kinds.as_slice().serialize(serializer)
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct RecorderStatus {
    pub enabled: bool,
    #[serde(serialize_with = "serialize_kinds")]
    pub kinds: [(EventKind, KindCounters); KIND_COUNT],
    pub owner_coverage: [OwnerCoverage; 8],
    pub loss: Loss,
}

impl Recorder {
    /// Explicit opt-in. Root is a dedicated bounded store, not an epoch directory.
    /// Each start creates a new epoch/key; existing epoch tapes are never appended.
    pub fn enable_opt_in(
        root: &Path,
        build: BuildIdentity,
        provenance: Provenance,
        at: Stamp,
    ) -> io::Result<(Self, Archive)> {
        Self::enable_with_limits(root, build, provenance, at, Limits::default())
    }
    pub fn enable_with_limits(
        root: &Path,
        build: BuildIdentity,
        provenance: Provenance,
        at: Stamp,
        limits: Limits,
    ) -> io::Result<(Self, Archive)> {
        let (store, epoch_root) = EpochStore::open(root, limits, at)?;
        let key: [u8; 32] = rand::random();
        let capture = pseudonym(&key, IdentityDomain::Operation, b"capture");
        let shared = Arc::new(Shared {
            capture,
            key,
            build,
            provenance,
            state: Mutex::new(State {
                enabled: false,
                next_sequence: 0,
                queue: VecDeque::new(),
                start: at,
                last: at,
                cursors: [None; 8],
                loss: Loss::default(),
                kinds: [KindCounters::default(); KIND_COUNT],
                coverage: [OwnerCoverage::default(); 8],
            }),
        });
        let mut archive = Archive {
            shared: Arc::clone(&shared),
            root: epoch_root,
            store,
            segments: VecDeque::new(),
            active: None,
            disk_bytes: 0,
            next_segment: 0,
            persisted_through: None,
            maintenance_mono: at.monotonic_ns,
            retention_evicted: 0,
            faulted: false,
            #[cfg(test)]
            io_test: ArchiveIoTest::default(),
        };
        archive.manifest(false)?; // An unsealed manifest exists BEFORE producers can enqueue.
        shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enabled = true;
        Ok((
            Self {
                shared: Some(shared),
            },
            archive,
        ))
    }
    pub fn identity(&self, domain: IdentityDomain, owner_identity: &[u8]) -> Option<Id> {
        if owner_identity.is_empty() || owner_identity.len() > 256 {
            if let Some(shared) = &self.shared {
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                let loss = &mut state.loss;
                increment(&mut loss.invalid, &mut loss.counters_saturated, 1);
            }
            return None;
        }
        self.shared
            .as_ref()
            .map(|s| pseudonym(&s.key, domain, owner_identity))
    }
    /// Call at owner visibility under the owner's existing lock; no I/O or callbacks.
    /// Enqueue order is NOT asserted to reconcile chronology across owner snapshots.
    pub fn record(
        &self,
        at: Stamp,
        owner: Owner,
        owner_cursor: u64,
        cause: Option<u64>,
        event: Event,
    ) -> RecordResult {
        let Some(shared) = &self.shared else {
            return RecordResult::Disabled;
        };
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.enabled {
            return RecordResult::Disabled;
        }
        let kind = event.kind();
        if matches!(kind, EventKind::Unclassified) {
            let loss = &mut state.loss;
            increment(&mut loss.unsupported_kind, &mut loss.counters_saturated, 1);
        }
        let counters = &mut state.kinds[kind as usize];
        increment(&mut counters.attempted, &mut counters.saturated, 1);
        let Some(sequence) = state.next_sequence.checked_add(1) else {
            let loss = &mut state.loss;
            increment(
                &mut loss.sequence_exhausted,
                &mut loss.counters_saturated,
                1,
            );
            state.reject(kind, owner);
            return RecordResult::Rejected;
        };
        state.next_sequence = sequence; // rejected records leave a visible sequence hole.
        let counters = &mut state.kinds[kind as usize];
        counters.first_sequence.get_or_insert(sequence);
        counters.last_sequence = Some(sequence);
        if at.monotonic_ns < state.last.monotonic_ns || at.utc_ns < state.last.utc_ns {
            let loss = &mut state.loss;
            increment(&mut loss.clock_regressed, &mut loss.counters_saturated, 1);
        }
        state.last.monotonic_ns = state.last.monotonic_ns.max(at.monotonic_ns);
        state.last.utc_ns = state.last.utc_ns.max(at.utc_ns);
        let prior = &mut state.cursors[owner as usize];
        let gap = prior.is_some_and(|p| p.checked_add(1) != Some(owner_cursor));
        *prior = Some(owner_cursor);
        let coverage = &mut state.coverage[owner as usize];
        coverage.first_cursor.get_or_insert(owner_cursor);
        coverage.last_cursor = Some(owner_cursor);
        if gap {
            increment(&mut coverage.cursor_gaps, &mut coverage.saturated, 1);
        }
        if gap {
            let loss = &mut state.loss;
            increment(&mut loss.owner_gap, &mut loss.counters_saturated, 1);
        }
        if !event.valid() || cause.is_some_and(|c| c >= sequence) {
            let loss = &mut state.loss;
            increment(&mut loss.invalid, &mut loss.counters_saturated, 1);
            state.reject(kind, owner);
            return RecordResult::Rejected;
        }
        let envelope = Envelope {
            schema: SCHEMA,
            capture: shared.capture,
            enqueue_sequence: sequence,
            at,
            owner,
            owner_cursor,
            cause_enqueue_sequence: cause,
            event,
        };
        let Ok(mut line) = serde_json::to_vec(&envelope) else {
            let loss = &mut state.loss;
            increment(&mut loss.invalid, &mut loss.counters_saturated, 1);
            state.reject(kind, owner);
            return RecordResult::Rejected;
        };
        line.push(b'\n');
        let counters = &mut state.kinds[kind as usize];
        increment(
            &mut counters.encoded_bytes,
            &mut counters.saturated,
            line.len() as u64,
        );
        if line.len() > MAX_RECORD {
            let loss = &mut state.loss;
            increment(&mut loss.oversized, &mut loss.counters_saturated, 1);
            state.reject(kind, owner);
            return RecordResult::Rejected;
        }
        if state.queue.len() == QUEUE_RECORDS {
            let loss = &mut state.loss;
            increment(&mut loss.queue_full, &mut loss.counters_saturated, 1);
            state.reject(kind, owner);
            return RecordResult::Rejected;
        }
        let counters = &mut state.kinds[kind as usize];
        increment(&mut counters.queued, &mut counters.saturated, 1);
        state.queue.push_back(Pending {
            kind,
            sequence,
            at,
            line,
        });
        RecordResult::Queued(sequence)
    }
    /// Constant-size producer/writer diagnostics; not a completeness receipt.
    pub fn status(&self) -> Option<RecorderStatus> {
        let shared = self.shared.as_ref()?;
        let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        Some(RecorderStatus {
            enabled: state.enabled,
            kinds: std::array::from_fn(|i| (EVENT_KINDS[i], state.kinds[i])),
            owner_coverage: state.coverage,
            loss: state.loss,
        })
    }
    /// Immediate off switch; accepted queued metadata is drained by the writer.
    pub fn disable(&self) {
        if let Some(shared) = &self.shared {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.enabled = false;
        }
    }
    /// Capture-hook failure, including when its event queue is already disabled
    /// or full. Constant-size status survives independently of event publication.
    pub(crate) fn producer_failed(&self) {
        if let Some(shared) = &self.shared {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let loss = &mut state.loss;
            increment(&mut loss.producer_failure, &mut loss.counters_saturated, 1);
            state.enabled = false;
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Segment {
    index: u64,
    first_sequence: u64,
    last_sequence: u64,
    first: Stamp,
    last: Stamp,
    records: u64,
    bytes: u64,
    sha256: Fingerprint,
}
struct Active {
    segment: Segment,
    hash: Sha256,
    file: fs::File,
}
#[derive(Serialize)]
enum CaptureScope {
    AccountAndCollectionMinimum,
}
#[derive(Serialize)]
enum StorageFormat {
    PlainJsonlV1,
}
#[derive(Serialize)]
struct Manifest<'a> {
    capture_scope: CaptureScope,
    storage: StorageFormat,
    schema: u16,
    identity_domain_version: u16,
    capture: Id,
    build: BuildIdentity,
    provenance: Provenance,
    start: Stamp,
    observed_through: Stamp,
    accepted_through: u64,
    persisted_through: Option<u64>,
    queued_records: usize,
    loss: Loss,
    #[serde(serialize_with = "serialize_kinds")]
    kinds: [(EventKind, KindCounters); KIND_COUNT],
    owner_coverage: [OwnerCoverage; 8],
    epoch_slot: usize,
    root_other_epoch_bytes: u64,
    root_reserved_bytes: u64,
    store_status: &'a StoreStatus,
    retention_ns: u64,
    disk_limit_bytes: u64,
    retained_bytes: u64,
    retention_evicted_records: u64,
    segments: &'a VecDeque<Segment>,
    active: Option<Segment>,
    sealed: bool,
    initial_state_reconciled: bool,
    owner_hooks_complete: bool,
    replay_ready: bool,
    actual_trace_accepted: bool,
    crash_loss_count: Option<u64>,
}

/// One non-Clone writer. The caller schedules flush/maintenance, including while disabled.
/// No filesystem write is performed from record(), disable(), or Drop.
pub struct Archive {
    shared: Arc<Shared>,
    root: PathBuf,
    store: EpochStore,
    segments: VecDeque<Segment>,
    active: Option<Active>,
    disk_bytes: u64,
    next_segment: u64,
    persisted_through: Option<u64>,
    maintenance_mono: u64,
    retention_evicted: u64,
    faulted: bool,
    #[cfg(test)]
    io_test: ArchiveIoTest,
}

// These hooks can stop or fail the real I/O boundary; they cannot grant receipts
// or replace a successful write/sync. Production has neither hooks nor counters.
#[cfg(test)]
#[derive(Default)]
struct ArchiveIoTest {
    opens: usize,
    writes: usize,
    syncs: usize,
    fail_write_at: Option<usize>,
    fail_sync_at: Option<usize>,
    fence: Option<ArchiveSyncFence>,
}
#[cfg(test)]
struct ArchiveSyncFence {
    at: usize,
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
#[cfg(test)]
impl ArchiveIoTest {
    fn before_write(&mut self, file: &mut fs::File, line: &[u8]) -> io::Result<()> {
        self.writes += 1;
        if self.fail_write_at == Some(self.writes) {
            file.write_all(&line[..line.len() / 2])?;
            return Err(io::Error::other("synthetic_archive_partial_write"));
        }
        Ok(())
    }
    fn before_sync(&mut self) -> io::Result<()> {
        self.syncs += 1;
        if self.fence.as_ref().is_some_and(|f| f.at == self.syncs) {
            let fence = self.fence.take().expect("matched sync fence");
            fence.entered.send(()).map_err(io::Error::other)?;
            fence
                .release
                .recv_timeout(std::time::Duration::from_secs(30))
                .map_err(io::Error::other)?;
        }
        if self.fail_sync_at == Some(self.syncs) {
            return Err(io::Error::other("synthetic_archive_sync_failure"));
        }
        Ok(())
    }
}
impl Archive {
    fn segment_path(&self, index: u64) -> PathBuf {
        self.root.join(format!("s{index:016x}.jsonl"))
    }
    fn storage_error(&mut self, cap: bool, count: u64) {
        self.faulted = true;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.enabled = false;
        let queued = state.queue.len() as u64;
        for pending in std::mem::take(&mut state.queue) {
            state.unpersisted(&pending);
        }
        let loss = &mut state.loss;
        if cap {
            increment(&mut loss.disk_cap, &mut loss.counters_saturated, count);
        } else {
            increment(&mut loss.write_error, &mut loss.counters_saturated, 1);
        }
        increment(
            &mut loss.unpersisted,
            &mut loss.counters_saturated,
            count.saturating_add(queued),
        );
    }
    fn seal_active(&mut self) {
        if let Some(active) = self.active.take() {
            let Active { segment, file, .. } = active;
            // In particular, close the handle before retention can remove it.
            drop(file);
            self.segments.push_back(segment);
        }
    }
    /// Commit one segment's admissible prefix from the already-bounded batch.
    /// A failed group can leave an unknown file tail, but grants no receipts.
    fn append_group(&mut self, batch: &VecDeque<Pending>, start: usize) -> io::Result<usize> {
        let pending = &batch[start];
        if self.active.as_ref().is_some_and(|a| {
            a.segment.bytes + pending.line.len() as u64 > SEGMENT_BYTES
                || pending
                    .at
                    .monotonic_ns
                    .saturating_sub(a.segment.first.monotonic_ns)
                    >= HOUR_NS
        }) {
            self.seal_active();
        }
        if !self.has_capacity(pending.line.len() as u64)
            || (self.active.is_none() && self.segments.len() >= MAX_SEGMENTS)
        {
            return Err(io::Error::other("capture_capacity"));
        }
        if self.active.is_none() {
            let next = self
                .next_segment
                .checked_add(1)
                .ok_or_else(|| io::Error::other("segment_ids"))?;
            self.next_segment = next;
            let file = OpenOptions::new()
                .append(true)
                .create_new(true)
                .open(self.segment_path(next))?;
            #[cfg(test)]
            {
                self.io_test.opens += 1;
            }
            self.active = Some(Active {
                segment: Segment {
                    index: next,
                    first_sequence: pending.sequence,
                    last_sequence: pending.sequence,
                    first: pending.at,
                    last: pending.at,
                    records: 0,
                    bytes: 0,
                    sha256: Fingerprint([0; 32]),
                },
                hash: Sha256::new(),
                file,
            });
        }
        // The first record fits by the checks above. Plan later records against
        // all prospective bytes, without advancing the durable checkpoint.
        let mut segment = self.active.as_ref().expect("active segment").segment;
        let mut bytes = pending.line.len() as u64;
        let mut end = start + 1;
        for pending in batch.iter().skip(end) {
            let next_bytes = bytes + pending.line.len() as u64;
            if segment.bytes + next_bytes > SEGMENT_BYTES
                || pending
                    .at
                    .monotonic_ns
                    .saturating_sub(segment.first.monotonic_ns)
                    >= HOUR_NS
                || !self.has_capacity(next_bytes)
            {
                break;
            }
            bytes = next_bytes;
            end += 1;
        }
        let active = self.active.as_mut().expect("active segment");
        let mut hash = active.hash.clone();
        for pending in batch.iter().take(end).skip(start) {
            #[cfg(test)]
            self.io_test.before_write(&mut active.file, &pending.line)?;
            active.file.write_all(&pending.line)?;
            hash.update(&pending.line);
            segment.bytes += pending.line.len() as u64;
            segment.records += 1;
            segment.last_sequence = pending.sequence;
            segment.last.monotonic_ns = segment.last.monotonic_ns.max(pending.at.monotonic_ns);
            segment.last.utc_ns = segment.last.utc_ns.max(pending.at.utc_ns);
        }
        #[cfg(test)]
        self.io_test.before_sync()?;
        active.file.sync_data()?;
        // Publish only the successfully synced group. In particular, no write
        // success, flush hint, or failed sync can advance these receipt fields.
        segment.sha256 = Fingerprint(hash.clone().finalize().into());
        active.segment = segment;
        active.hash = hash;
        self.disk_bytes += bytes;
        self.persisted_through = Some(segment.last_sequence);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        for pending in batch.iter().take(end).skip(start) {
            let counters = &mut state.kinds[pending.kind as usize];
            increment(&mut counters.persisted, &mut counters.saturated, 1);
            increment(
                &mut counters.persisted_bytes,
                &mut counters.saturated,
                pending.line.len() as u64,
            );
            counters
                .first_persisted_sequence
                .get_or_insert(pending.sequence);
            counters.last_persisted_sequence = Some(pending.sequence);
        }
        Ok(end)
    }
    fn has_capacity(&self, bytes: u64) -> bool {
        self.disk_bytes
            .checked_add(self.store.other_bytes)
            .and_then(|n| n.checked_add(STORE_RESERVE))
            .and_then(|n| n.checked_add(bytes))
            .is_some_and(|n| n <= self.store.limits.disk_bytes)
    }
    /// Filesystem operations occur only here, on the sole writer, outside producer locks.
    pub fn flush(&mut self, now: Stamp) -> io::Result<()> {
        self.maintain(now)?;
        if self.faulted {
            return Err(io::Error::other("capture_faulted"));
        }
        let batch = {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut state.queue)
        };
        let count = batch.len();
        let mut committed = 0;
        while committed < count {
            match self.append_group(&batch, committed) {
                Ok(end) => committed = end,
                Err(error) => {
                    let cap = !self.has_capacity(batch[committed].line.len() as u64)
                        || (self.active.is_none() && self.segments.len() >= MAX_SEGMENTS);
                    {
                        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                        // Include every write in the unconfirmed group, not just
                        // the record whose write/sync reported the failure.
                        for pending in batch.iter().skip(committed) {
                            state.unpersisted(pending);
                        }
                    }
                    self.storage_error(cap, (count - committed) as u64);
                    let _ = self.manifest(false);
                    return Err(error);
                }
            }
        }
        self.maintain(now)?;
        self.manifest(false)
    }
    /// Seven days by injected process-monotonic time; never delete young data to fit a cap.
    /// Maintenance must be scheduled by runtime; offline processes cannot delete files.
    pub fn maintain(&mut self, now: Stamp) -> io::Result<()> {
        let root_result = self.store.maintain(now);
        if root_result.is_err() {
            self.storage_error(false, 0);
        }
        // Root UTC ambiguity must stop capture and old-epoch deletion, but it
        // cannot invalidate this process's known monotonic ages and paths.
        if now.monotonic_ns < self.maintenance_mono {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let loss = &mut state.loss;
            increment(&mut loss.clock_regressed, &mut loss.counters_saturated, 1);
        }
        self.maintenance_mono = self.maintenance_mono.max(now.monotonic_ns);
        if self.active.as_ref().is_some_and(|a| {
            self.maintenance_mono
                .saturating_sub(a.segment.first.monotonic_ns)
                >= RETENTION_NS
        }) {
            self.seal_active();
        }
        while let Some(segment) = self.segments.front().copied() {
            if self
                .maintenance_mono
                .saturating_sub(segment.first.monotonic_ns)
                < RETENTION_NS
            {
                break;
            }
            if let Err(error) = fs::remove_file(self.segment_path(segment.index)) {
                self.storage_error(false, 0);
                return Err(error);
            }
            self.segments.pop_front();
            self.disk_bytes -= segment.bytes;
            self.retention_evicted = self.retention_evicted.saturating_add(segment.records);
            // Expire whole bounded segments. If their tail is newer than the cutoff,
            // explicitly report that boundary loss instead of claiming a complete tape.
            if self
                .maintenance_mono
                .saturating_sub(segment.last.monotonic_ns)
                < RETENTION_NS
            {
                let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                let loss = &mut state.loss;
                increment(
                    &mut loss.retention_boundary_segments,
                    &mut loss.counters_saturated,
                    1,
                );
            }
        }
        root_result
    }
    fn manifest(&mut self, sealed: bool) -> io::Result<()> {
        let result = self.write_manifest(sealed);
        if result.is_err() {
            self.storage_error(false, 0);
        }
        result
    }
    fn write_manifest(&mut self, sealed: bool) -> io::Result<()> {
        let bytes = {
            let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            serde_json::to_vec(&Manifest {
                capture_scope: CaptureScope::AccountAndCollectionMinimum,
                storage: StorageFormat::PlainJsonlV1,
                schema: SCHEMA,
                identity_domain_version: ID_DOMAIN_VERSION,
                capture: self.shared.capture,
                build: self.shared.build,
                provenance: self.shared.provenance,
                start: state.start,
                observed_through: state.last,
                accepted_through: state.next_sequence,
                persisted_through: self.persisted_through,
                queued_records: state.queue.len(),
                loss: state.loss,
                kinds: std::array::from_fn(|i| (EVENT_KINDS[i], state.kinds[i])),
                owner_coverage: state.coverage,
                epoch_slot: self.store.slot,
                root_other_epoch_bytes: self.store.other_bytes,
                root_reserved_bytes: STORE_RESERVE,
                store_status: &self.store.status,
                retention_ns: RETENTION_NS,
                disk_limit_bytes: self.store.limits.disk_bytes,
                retained_bytes: self.disk_bytes,
                retention_evicted_records: self.retention_evicted,
                segments: &self.segments,
                active: self.active.as_ref().map(|a| a.segment),
                sealed,
                initial_state_reconciled: false,
                owner_hooks_complete: false,
                replay_ready: false,
                actual_trace_accepted: false,
                crash_loss_count: if sealed { Some(0) } else { None },
            })
            .map_err(io::Error::other)?
        };
        if bytes.len() as u64 > MANIFEST_RESERVE / 2 {
            return Err(io::Error::other("manifest_capacity"));
        }
        let temporary = self.root.join("manifest.pending");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(temporary, self.root.join("manifest.json"))
    }
    pub fn close(mut self, now: Stamp) -> io::Result<()> {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.enabled = false;
        }
        self.flush(now)?;
        self.seal_active();
        self.manifest(true)
    }
}
impl Drop for Archive {
    fn drop(&mut self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enabled = false;
        // Existing on-disk unsealed manifest remains evidence of an unclean close.
    }
}

/// Optional module-local scheduler. Main owns enable/config and passes an actual
/// clock; no worker exists unless explicitly spawned. disable() keeps maintenance
/// running; shutdown() drains and joins. Drop signals shutdown without blocking.
pub struct PeriodicWriter {
    recorder: Recorder,
    stop: Option<std::sync::mpsc::SyncSender<()>>,
    join: Option<std::thread::JoinHandle<io::Result<()>>>,
}
impl Archive {
    pub fn spawn_periodic(
        mut self,
        clock: Arc<dyn Fn() -> Stamp + Send + Sync>,
        interval: std::time::Duration,
    ) -> io::Result<PeriodicWriter> {
        if !(std::time::Duration::from_millis(100)..=std::time::Duration::from_secs(60))
            .contains(&interval)
        {
            return Err(io::Error::other("capture_flush_interval"));
        }
        let recorder = Recorder {
            shared: Some(Arc::clone(&self.shared)),
        };
        let (stop, receive) = std::sync::mpsc::sync_channel(1);
        let join = std::thread::Builder::new()
            .name("qb-replay-writer".into())
            .spawn(move || {
                loop {
                    match receive.recv_timeout(interval) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            return self.close(clock());
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            let now = clock();
                            if self.faulted {
                                // Preserve the faulted/unsealed result; still expire stored data.
                                let _ = self.maintain(now);
                            } else {
                                let _ = self.flush(now);
                            }
                        }
                    }
                }
            })?;
        Ok(PeriodicWriter {
            recorder,
            stop: Some(stop),
            join: Some(join),
        })
    }
}
impl PeriodicWriter {
    pub fn disable(&self) {
        self.recorder.disable();
    }
    pub fn shutdown(mut self) -> io::Result<()> {
        self.disable();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.join
            .take()
            .expect("writer join")
            .join()
            .map_err(|_| io::Error::other("capture_writer_panicked"))?
    }
}
impl Drop for PeriodicWriter {
    fn drop(&mut self) {
        self.disable();
        if let Some(stop) = self.stop.take() {
            let _ = stop.try_send(());
        }
        // Explicit shutdown is required for observed durability/errors. Detached
        // shutdown may be interrupted by process exit, leaving an unsealed tape.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replay_producer_failure_remains_visible_after_disable_without_a_queue_event() {
        let off = Recorder::default();
        off.producer_failed();
        assert!(off.status().is_none());
        let dir = fixture_dir();
        let root = dir.path().join("producer-loss");
        let (recorder, archive) =
            Recorder::enable_opt_in(&root, build(), Provenance::Synthetic, stamp(0)).unwrap();
        recorder.disable();
        recorder.producer_failed();
        let status = recorder.status().unwrap();
        assert!(!status.enabled);
        assert_eq!(status.loss.producer_failure, 1);
        assert!(status.kinds.iter().all(|(_, count)| count.queued == 0));
        archive.close(stamp(1)).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("e00/manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["loss"]["producer_failure"], 1);
        assert_eq!(manifest["replay_ready"], false);
    }
    #[test]
    fn minimum_schema_classifies_actual_producers_without_claiming_owner_coverage() {
        use super::super::replay_projection::{
            AccountFacts, HealthKind, Scope, State as AccountState,
        };
        let (_dir, recorder, archive) = setup();
        let account = recorder
            .identity(IdentityDomain::Account, b"synthetic-account")
            .unwrap();
        let facts = AccountFacts {
            account,
            provider: Provider::Codex,
            order_rank: Some(0),
            generation: 1,
            state: AccountState::Active,
            health: HealthKind::Ready,
            reason: None,
            scope: Scope::Account,
            health_until_utc_ns: None,
            health_since_utc_ns: None,
            auth_failures: 0,
            payment_failures: 0,
            payment_until_utc_ns: None,
            auth_probe_pending: false,
            cooling_window: None,
            cooling_reset_utc_ns: None,
            cooling_started_utc_ns: None,
            quota_credit_releasable: false,
            drain: false,
            allow_credits: false,
            expires_utc_ns: None,
            refresh_uncertain: false,
            credential_available: None,
        };
        let events = [
            Event::AccountState { facts },
            Event::CollectionBoundary {
                collection: 1,
                started: Some(stamp(0)),
                rows: 1,
                complete: false,
            },
            Event::CollectionMember {
                collection: 1,
                account,
                side: CollectionSide::SourceAndTarget,
                started: stamp(0),
                fact_revision: None,
                order_rank: 0,
                exclusion: Exclusion::None,
            },
            event(&recorder),
        ];
        for (i, event) in events.into_iter().enumerate() {
            assert!(matches!(
                recorder.record(
                    stamp(i as u64 + 1),
                    Owner::Collector,
                    i as u64 + 1,
                    None,
                    event
                ),
                RecordResult::Queued(_)
            ));
        }
        let epoch = archive.root.clone();
        archive.close(stamp(4)).unwrap();
        let status = recorder.status().unwrap();
        assert_eq!(status.kinds.len(), 11);
        assert_eq!(status.owner_coverage.len(), 8);
        assert_eq!(status.kinds[EventKind::Account as usize].1.persisted, 2);
        assert_eq!(
            status.kinds[EventKind::CollectionMember as usize]
                .1
                .persisted,
            2
        );
        assert_eq!(
            status.kinds[EventKind::Unclassified as usize].1.attempted,
            0
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(epoch.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["schema"], 3);
        assert_eq!(manifest["capture_scope"], "AccountAndCollectionMinimum");
        assert_eq!(manifest["storage"], "PlainJsonlV1");
        for flag in [
            "initial_state_reconciled",
            "owner_hooks_complete",
            "replay_ready",
            "actual_trace_accepted",
        ] {
            assert_eq!(manifest[flag], false);
        }
    }

    #[test]
    fn minimum_schema_never_reinterprets_a_preserved_schema_two_epoch() {
        let (_dir, _recorder, archive) = setup();
        let epoch = archive.root.clone();
        let root = archive.store.root.clone();
        archive.close(stamp(1)).unwrap();
        let header = serde_json::json!({"schema": 2, "start_utc_ns": 0});
        fs::write(
            epoch.join("epoch.json"),
            serde_json::to_vec(&header).unwrap(),
        )
        .unwrap();
        assert!(Recorder::enable_opt_in(&root, build(), Provenance::Synthetic, stamp(2)).is_err());
        assert!(
            epoch.exists(),
            "incompatible tape must not be deleted or reused"
        );
    }

    #[test]
    fn limits_validate_default_and_both_bounds() {
        assert_eq!(Limits::default().disk_bytes(), 256 * 1024 * 1024);
        for bytes in [16 * 1024 * 1024, 512 * 1024 * 1024] {
            assert_eq!(Limits::new(bytes).unwrap().disk_bytes(), bytes);
        }
        assert!(Limits::new(16 * 1024 * 1024 - 1).is_err());
        assert!(Limits::new(512 * 1024 * 1024 + 1).is_err());
    }
    #[test]
    fn restart_keeps_young_epochs_locked_bounded_and_marks_expiry_loss() {
        let directory = fixture_dir();
        let root = directory.path().join("store");
        let (_, archive) =
            Recorder::enable_opt_in(&root, build(), Provenance::Synthetic, stamp(0)).unwrap();
        assert!(Recorder::enable_opt_in(&root, build(), Provenance::Synthetic, stamp(0)).is_err());
        archive.close(stamp(0)).unwrap();
        for n in 1..MAX_EPOCHS as u64 {
            let (_, archive) =
                Recorder::enable_opt_in(&root, build(), Provenance::Synthetic, stamp(n)).unwrap();
            assert_eq!(archive.store.slot, n as usize);
            archive.close(stamp(n)).unwrap();
        }
        assert!(
            Recorder::enable_opt_in(
                &root,
                build(),
                Provenance::Synthetic,
                stamp(MAX_EPOCHS as u64)
            )
            .is_err()
        );
        let at = stamp(RETENTION_NS + MAX_EPOCHS as u64);
        let (_, archive) =
            Recorder::enable_opt_in(&root, build(), Provenance::Synthetic, at).unwrap();
        assert_eq!(archive.store.slot, 0);
        assert_eq!(
            archive.store.status.expiry_removal_attempts,
            MAX_EPOCHS as u64
        );
        assert!(archive.store.status.expired_epoch_loss_unknown);
        archive.close(at).unwrap();
    }
    #[test]
    fn restart_counts_other_epochs_in_one_root_cap_and_rejects_regressed_clock() {
        let directory = fixture_dir();
        let root = directory.path().join("store");
        let limits = Limits::new(16 * 1024 * 1024).unwrap();
        let (_, archive) =
            Recorder::enable_with_limits(&root, build(), Provenance::Synthetic, stamp(10), limits)
                .unwrap();
        let epoch = archive.root.clone();
        archive.close(stamp(10)).unwrap();
        // Synthetic sparse regular files exercise logical root accounting without
        // writing a large payload or pretending these are accepted replay records.
        for n in 1..=6 {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(epoch.join(format!("s{n:016x}.jsonl")))
                .unwrap();
            file.set_len(SEGMENT_BYTES).unwrap();
        }
        assert!(
            Recorder::enable_with_limits(&root, build(), Provenance::Synthetic, stamp(11), limits)
                .is_err()
        );
        assert!(Recorder::enable_opt_in(&root, build(), Provenance::Synthetic, stamp(9)).is_err());
        let (_, archive) =
            Recorder::enable_opt_in(&root, build(), Provenance::Synthetic, stamp(11)).unwrap();
        assert!(archive.store.other_bytes >= 6 * SEGMENT_BYTES);
        archive.close(stamp(11)).unwrap();
    }
    #[test]
    fn foreign_entries_and_partial_epoch_headers_fail_closed() {
        let directory = fixture_dir();
        let root = directory.path().join("store");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("unexpected"), b"synthetic").unwrap();
        assert!(Recorder::enable_opt_in(&root, build(), Provenance::Synthetic, stamp(0)).is_err());
        fs::remove_file(root.join("unexpected")).unwrap();
        fs::create_dir(root.join("e00")).unwrap();
        assert!(Recorder::enable_opt_in(&root, build(), Provenance::Synthetic, stamp(0)).is_err());
    }
    #[test]
    fn per_kind_bytes_and_loss_distinguish_attempt_queue_and_persistence() {
        let (_directory, recorder, mut archive) = setup();
        for n in 1..=QUEUE_RECORDS as u64 + 1 {
            recorder.record(stamp(n), Owner::Runtime, n, None, event(&recorder));
        }
        let before = recorder.status().unwrap();
        let account = before.kinds[EventKind::Account as usize].1;
        assert_eq!(account.attempted, QUEUE_RECORDS as u64 + 1);
        assert_eq!(account.queued, QUEUE_RECORDS as u64);
        assert_eq!(account.rejected, 1);
        assert_eq!(account.persisted, 0);
        archive.flush(stamp(QUEUE_RECORDS as u64 + 1)).unwrap();
        let account = recorder.status().unwrap().kinds[EventKind::Account as usize].1;
        assert_eq!(account.persisted, QUEUE_RECORDS as u64);
        assert_eq!(account.persisted_bytes, archive.disk_bytes);
        assert!(account.encoded_bytes > account.persisted_bytes);
        assert_eq!(account.first_persisted_sequence, Some(1));
        assert_eq!(account.last_persisted_sequence, Some(QUEUE_RECORDS as u64));
    }
    #[test]
    fn periodic_writer_shutdown_drains_and_observes_off_without_sleep() {
        let (_directory, recorder, archive) = setup();
        let epoch = archive.root.clone();
        recorder.record(stamp(1), Owner::Runtime, 1, None, event(&recorder));
        let clock: Arc<dyn Fn() -> Stamp + Send + Sync> = Arc::new(|| stamp(2));
        let writer = archive
            .spawn_periodic(clock, std::time::Duration::from_secs(60))
            .unwrap();
        writer.disable();
        assert_eq!(
            recorder.record(stamp(2), Owner::Runtime, 2, None, event(&recorder)),
            RecordResult::Disabled
        );
        writer.shutdown().unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(epoch.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["sealed"], true);
        assert_eq!(
            manifest["kinds"][EventKind::Account as usize][1]["persisted"],
            1
        );
        assert_eq!(manifest["initial_state_reconciled"], false);
    }
    #[test]
    fn pseudonym_matches_independent_stdlib_hmac_vector() {
        assert_eq!(
            pseudonym(&[11; 32], IdentityDomain::Session, b"synthetic-session"),
            Id([
                67, 45, 243, 59, 131, 170, 45, 164, 10, 82, 9, 85, 209, 241, 95, 218, 211, 90, 254,
                79, 249, 254, 199, 150, 93, 105, 192, 120, 110, 61, 252, 31
            ])
        );
    }
    fn stamp(n: u64) -> Stamp {
        Stamp {
            utc_ns: n as i64,
            monotonic_ns: n,
        }
    }
    fn build() -> BuildIdentity {
        let h = Fingerprint([1; 32]);
        BuildIdentity {
            installed_executable: Some(h),
            installed_source: Some(h),
            candidate_source: None,
            installed_build: Some(h),
            candidate_build: None,
            config: Some(h),
            toolchain: Some(h),
            dependencies: Some(h),
            installed_adapter: Some(h),
            candidate_adapter: None,
            projection: Some(h),
            recorder: Some(h),
        }
    }
    #[test]
    fn unknown_build_metadata_is_null_and_executable_does_not_attest_source() {
        let unknown = serde_json::to_value(BuildIdentity::default()).unwrap();
        let fields = unknown.as_object().unwrap();
        assert_eq!(fields.len(), 12);
        assert!(fields.values().all(serde_json::Value::is_null));
        let executable_only = BuildIdentity {
            installed_executable: Some(Fingerprint([7; 32])),
            ..BuildIdentity::default()
        };
        let encoded = serde_json::to_value(executable_only).unwrap();
        assert!(encoded["installed_executable"].is_array());
        for (name, value) in encoded.as_object().unwrap() {
            if name != "installed_executable" {
                assert!(value.is_null(), "executable digest cannot attest {name}");
            }
        }
    }
    fn setup() -> (tempfile::TempDir, Recorder, Archive) {
        let directory = fixture_dir();
        let (recorder, archive) = Recorder::enable_opt_in(
            &directory.path().join("capture"),
            build(),
            Provenance::Synthetic,
            stamp(0),
        )
        .unwrap();
        (directory, recorder, archive)
    }
    fn fixture_dir() -> tempfile::TempDir {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r142-recorder-fixtures");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }
    fn event(recorder: &Recorder) -> Event {
        Event::Account {
            account: recorder
                .identity(IdentityDomain::Account, b"synthetic-secret-email")
                .unwrap(),
            provider: Provider::Codex,
            order_rank: 0,
            revision: 1,
            eligible: Some(true),
            credential_available: true,
            drain: false,
            origin: Origin::IndependentPoll,
        }
    }
    fn archive_sync_fence(
        archive: &mut Archive,
        at: usize,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let (entered, arrival) = std::sync::mpsc::sync_channel(1);
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        archive.io_test.fence = Some(ArchiveSyncFence {
            at,
            entered,
            release: resume,
        });
        (arrival, release)
    }

    fn archive_queued_lines(recorder: &Recorder) -> Vec<Vec<u8>> {
        let state = recorder.shared.as_ref().unwrap().state.lock().unwrap();
        state.queue.iter().map(|p| p.line.clone()).collect()
    }

    fn archive_assert_unclean(root: &Path) {
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["sealed"], false);
        assert_eq!(manifest["crash_loss_count"], serde_json::Value::Null);
        for field in [
            "initial_state_reconciled",
            "owner_hooks_complete",
            "replay_ready",
            "actual_trace_accepted",
        ] {
            assert_eq!(manifest[field], false);
        }
    }

    #[test]
    fn r141_archive_group_sync_receipts_wait_for_real_sync() {
        let (_dir, recorder, mut archive) = setup();
        for n in 1..=8 {
            assert_eq!(
                recorder.record(stamp(n), Owner::Runtime, n, None, event(&recorder)),
                RecordResult::Queued(n)
            );
        }
        let expected = archive_queued_lines(&recorder).concat();
        let path = archive.segment_path(1);
        let (entered, release) = archive_sync_fence(&mut archive, 1);
        let worker = std::thread::spawn(move || {
            let result = archive.flush(stamp(8));
            (archive, result)
        });
        entered
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        // Real write_all calls have completed, immediately before real sync_data.
        assert_eq!(fs::read(&path).unwrap(), expected);
        let status = recorder.status().unwrap();
        assert!(status.enabled);
        assert!(status.kinds.iter().all(|(_, c)| {
            c.persisted == 0
                && c.persisted_bytes == 0
                && c.first_persisted_sequence.is_none()
                && c.last_persisted_sequence.is_none()
        }));
        // The storage fence holds no producer State lock; this stays in the next
        // bounded queue and must not be accidentally credited by this flush.
        assert_eq!(
            recorder.record(stamp(9), Owner::Runtime, 9, None, event(&recorder)),
            RecordResult::Queued(9)
        );
        release.send(()).unwrap();
        let (mut archive, result) = worker.join().unwrap();
        result.unwrap();
        assert_eq!(archive.io_test.opens, 1);
        assert_eq!(archive.io_test.syncs, 1);
        assert_eq!(archive.io_test.writes, 8);
        assert_eq!(archive.persisted_through, Some(8));
        let segment = archive.active.as_ref().unwrap().segment;
        assert_eq!(segment.records, 8);
        assert_eq!(segment.bytes, expected.len() as u64);
        assert_eq!(
            segment.sha256,
            Fingerprint(Sha256::digest(&expected).into())
        );
        let counts = recorder.status().unwrap().kinds[EventKind::Account as usize].1;
        assert_eq!(counts.queued, 9);
        assert_eq!(counts.persisted, 8);
        assert_eq!(counts.last_persisted_sequence, Some(8));
        let mut complete = expected;
        complete.extend(archive_queued_lines(&recorder).concat());
        archive.flush(stamp(9)).unwrap();
        assert_eq!(archive.io_test.opens, 1, "active file must stay open");
        assert_eq!(archive.io_test.syncs, 2);
        assert_eq!(fs::read(path).unwrap(), complete);
        let segment = archive.active.as_ref().unwrap().segment;
        assert_eq!(segment.records, 9);
        assert_eq!(
            segment.sha256,
            Fingerprint(Sha256::digest(&complete).into())
        );
        assert_eq!(archive.persisted_through, Some(9));
        archive.close(stamp(9)).unwrap();
    }

    #[test]
    fn r141_archive_group_sync_failure_keeps_prior_groups_and_counts_entire_tail() {
        let (_dir, recorder, mut archive) = setup();
        let times = [1, 2, HOUR_NS + 1, HOUR_NS + 2, 2 * HOUR_NS + 1];
        for (i, at) in times.into_iter().enumerate() {
            let n = i as u64 + 1;
            assert_eq!(
                recorder.record(stamp(at), Owner::Runtime, n, None, event(&recorder)),
                RecordResult::Queued(n)
            );
        }
        let lines = archive_queued_lines(&recorder);
        let durable = lines[..2].concat();
        let unconfirmed = lines[2..4].concat();
        let root = archive.root.clone();
        let first_path = archive.segment_path(1);
        let second_path = archive.segment_path(2);
        archive.io_test.fail_sync_at = Some(2);
        let (entered, release) = archive_sync_fence(&mut archive, 2);
        let worker = std::thread::spawn(move || {
            let result = archive.flush(stamp(2 * HOUR_NS + 1));
            (archive, result)
        });
        entered
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        assert_eq!(fs::read(&first_path).unwrap(), durable);
        assert_eq!(fs::read(&second_path).unwrap(), unconfirmed);
        let counts = recorder.status().unwrap().kinds[EventKind::Account as usize].1;
        assert_eq!(counts.persisted, 2);
        assert_eq!(counts.last_persisted_sequence, Some(2));
        // This concurrent accepted record is outside the drained batch. It must
        // be charged once along with all three unconfirmed drained records.
        assert_eq!(
            recorder.record(
                stamp(2 * HOUR_NS + 2),
                Owner::Runtime,
                6,
                None,
                event(&recorder)
            ),
            RecordResult::Queued(6)
        );
        release.send(()).unwrap();
        let (archive, result) = worker.join().unwrap();
        assert_eq!(
            result.unwrap_err().to_string(),
            "synthetic_archive_sync_failure"
        );
        let status = recorder.status().unwrap();
        assert!(!status.enabled);
        let counts = status.kinds[EventKind::Account as usize].1;
        assert_eq!(counts.queued, 6);
        assert_eq!(counts.persisted, 2);
        assert_eq!(counts.persisted_bytes, durable.len() as u64);
        assert_eq!(counts.unpersisted, 4);
        assert_eq!(counts.first_persisted_sequence, Some(1));
        assert_eq!(counts.last_persisted_sequence, Some(2));
        assert_eq!(status.loss.unpersisted, 4);
        assert_eq!(status.loss.write_error, 1);
        assert_eq!(status.loss.disk_cap, 0);
        assert_eq!(archive.persisted_through, Some(2));
        assert_eq!(archive.disk_bytes, durable.len() as u64);
        assert_eq!(archive.io_test.syncs, 2);
        assert_eq!(archive.io_test.opens, 2);
        assert_eq!(archive.io_test.writes, 4);
        assert_eq!(archive.segments.len(), 1);
        assert_eq!(archive.segments[0].records, 2);
        assert_eq!(
            archive.segments[0].sha256,
            Fingerprint(Sha256::digest(&durable).into())
        );
        assert_eq!(archive.active.as_ref().unwrap().segment.records, 0);
        assert_eq!(archive.active.as_ref().unwrap().segment.bytes, 0);
        assert!(!archive.segment_path(3).exists());
        archive_assert_unclean(&root);
        assert!(archive.close(stamp(2 * HOUR_NS + 2)).is_err());
        assert_eq!(recorder.status().unwrap().loss.unpersisted, 4);
        archive_assert_unclean(&root);
    }

    #[test]
    fn r141_archive_group_partial_write_keeps_same_segment_checkpoint() {
        let (_dir, recorder, mut archive) = setup();
        for n in 1..=2 {
            assert_eq!(
                recorder.record(stamp(n), Owner::Runtime, n, None, event(&recorder)),
                RecordResult::Queued(n)
            );
        }
        archive.flush(stamp(2)).unwrap();
        let checkpoint = archive.active.as_ref().unwrap().segment;
        let path = archive.segment_path(1);
        let mut physical = fs::read(&path).unwrap();
        let durable_bytes = physical.len() as u64;
        for n in 3..=6 {
            assert_eq!(
                recorder.record(stamp(n), Owner::Runtime, n, None, event(&recorder)),
                RecordResult::Queued(n)
            );
        }
        let lines = archive_queued_lines(&recorder);
        physical.extend_from_slice(&lines[0]);
        physical.extend_from_slice(&lines[1][..lines[1].len() / 2]);
        // Two previous writes succeeded; fail halfway through this group's
        // second line, after its first full line was written without sync.
        archive.io_test.fail_write_at = Some(4);
        assert_eq!(
            archive.flush(stamp(6)).unwrap_err().to_string(),
            "synthetic_archive_partial_write"
        );
        assert_eq!(fs::read(&path).unwrap(), physical);
        let after = archive.active.as_ref().unwrap().segment;
        assert_eq!(after.bytes, checkpoint.bytes);
        assert_eq!(after.records, checkpoint.records);
        assert_eq!(after.last_sequence, checkpoint.last_sequence);
        assert_eq!(after.last.monotonic_ns, checkpoint.last.monotonic_ns);
        assert_eq!(after.last.utc_ns, checkpoint.last.utc_ns);
        assert_eq!(after.sha256, checkpoint.sha256);
        assert_eq!(archive.disk_bytes, durable_bytes);
        assert_eq!(archive.persisted_through, Some(2));
        assert_eq!(archive.io_test.opens, 1);
        assert_eq!(archive.io_test.writes, 4);
        assert_eq!(archive.io_test.syncs, 1);
        let status = recorder.status().unwrap();
        assert!(!status.enabled);
        let counts = status.kinds[EventKind::Account as usize].1;
        assert_eq!(counts.persisted, 2);
        assert_eq!(counts.persisted_bytes, durable_bytes);
        assert_eq!(counts.last_persisted_sequence, Some(2));
        assert_eq!(counts.unpersisted, 4);
        assert_eq!(status.loss.unpersisted, 4);
        assert_eq!(status.loss.write_error, 1);
        assert_eq!(status.loss.disk_cap, 0);
        let root = archive.root.clone();
        archive_assert_unclean(&root);
        assert!(archive.close(stamp(6)).is_err());
        assert_eq!(recorder.status().unwrap().loss.unpersisted, 4);
        archive_assert_unclean(&root);
    }

    #[test]
    fn r141_archive_group_cap_preserves_synced_admissible_prefix() {
        let (_dir, recorder, mut archive) = setup();
        for n in 1..=4 {
            assert_eq!(
                recorder.record(stamp(n), Owner::Runtime, n, None, event(&recorder)),
                RecordResult::Queued(n)
            );
        }
        let prefix = archive_queued_lines(&recorder)[..2].concat();
        // Like the existing disk-cap test, model already charged logical bytes
        // without making a large payload. Defaults and limits stay unchanged.
        let charged = archive.store.limits.disk_bytes - STORE_RESERVE - prefix.len() as u64;
        archive.disk_bytes = charged;
        assert!(archive.flush(stamp(4)).is_err());
        let path = archive.segment_path(1);
        assert_eq!(fs::read(&path).unwrap(), prefix);
        assert_eq!(archive.io_test.opens, 1);
        assert_eq!(archive.io_test.syncs, 1);
        assert_eq!(archive.io_test.writes, 2);
        assert_eq!(archive.persisted_through, Some(2));
        assert_eq!(archive.disk_bytes, charged + prefix.len() as u64);
        let segment = archive.active.as_ref().unwrap().segment;
        assert_eq!(segment.bytes, prefix.len() as u64);
        assert_eq!(segment.records, 2);
        assert_eq!(segment.sha256, Fingerprint(Sha256::digest(&prefix).into()));
        let status = recorder.status().unwrap();
        assert!(!status.enabled);
        let counts = status.kinds[EventKind::Account as usize].1;
        assert_eq!(counts.persisted, 2);
        assert_eq!(counts.unpersisted, 2);
        assert_eq!(status.loss.disk_cap, 2);
        assert_eq!(status.loss.write_error, 0);
        assert_eq!(status.loss.unpersisted, 2);
        archive.maintain(stamp(100)).unwrap();
        assert!(path.exists(), "capacity must not evict young segment data");
        let root = archive.root.clone();
        assert!(archive.close(stamp(100)).is_err());
        archive_assert_unclean(&root);
    }

    #[test]
    fn r141_archive_group_real_byte_hour_rotation_and_active_retention() {
        let (_dir, recorder, mut archive) = setup();
        let mut cursor = 0;
        let mut flushes = 0;
        // Every accepted record is real encoded metadata. At most eight bounded
        // batches are enough to exercise the unchanged 2 MiB segment limit.
        for _ in 0..8 {
            for _ in 0..QUEUE_RECORDS {
                cursor += 1;
                assert_eq!(
                    recorder.record(
                        stamp(cursor),
                        Owner::Runtime,
                        cursor,
                        None,
                        event(&recorder)
                    ),
                    RecordResult::Queued(cursor)
                );
            }
            archive.flush(stamp(cursor)).unwrap();
            flushes += 1;
            if !archive.segments.is_empty() {
                break;
            }
        }
        assert_eq!(archive.segments.len(), 1, "real byte rollover must occur");
        assert_eq!(archive.io_test.opens, 2);
        assert!((flushes..=flushes + 1).contains(&archive.io_test.syncs));
        let first = fs::read(archive.segment_path(1)).unwrap();
        let second = fs::read(archive.segment_path(2)).unwrap();
        let first_segment = archive.segments[0];
        let second_segment = archive.active.as_ref().unwrap().segment;
        let next_line_bytes = second.iter().position(|b| *b == b'\n').unwrap() + 1;
        assert!(first.len() as u64 <= SEGMENT_BYTES);
        assert!(first.len() as u64 + next_line_bytes as u64 > SEGMENT_BYTES);
        assert_eq!(
            first_segment.sha256,
            Fingerprint(Sha256::digest(&first).into())
        );
        assert_eq!(
            second_segment.sha256,
            Fingerprint(Sha256::digest(&second).into())
        );
        assert_eq!(
            second_segment.first_sequence,
            first_segment.last_sequence + 1
        );
        assert_eq!(first_segment.records + second_segment.records, cursor);
        assert_eq!(archive.disk_bytes, (first.len() + second.len()) as u64);
        assert_eq!(archive.persisted_through, Some(cursor));
        let mut expected_sequence = 1;
        for line in first
            .split(|b| *b == b'\n')
            .chain(second.split(|b| *b == b'\n'))
        {
            if !line.is_empty() {
                let record: serde_json::Value = serde_json::from_slice(line).unwrap();
                assert_eq!(record["enqueue_sequence"], expected_sequence);
                expected_sequence += 1;
            }
        }
        assert_eq!(expected_sequence, cursor + 1);
        cursor += 1;
        let hour = HOUR_NS + cursor;
        assert_eq!(
            recorder.record(stamp(hour), Owner::Runtime, cursor, None, event(&recorder)),
            RecordResult::Queued(cursor)
        );
        archive.flush(stamp(hour)).unwrap();
        assert_eq!(archive.io_test.opens, 3);
        assert_eq!(archive.segments.len(), 2);
        assert_eq!(archive.active.as_ref().unwrap().segment.index, 3);
        assert_eq!(archive.active.as_ref().unwrap().segment.records, 1);
        let counts = recorder.status().unwrap().kinds[EventKind::Account as usize].1;
        assert_eq!(counts.persisted, cursor);
        assert_eq!(counts.unpersisted, 0);
        archive.maintain(stamp(hour + RETENTION_NS)).unwrap();
        assert!(archive.active.is_none());
        assert!(archive.segments.is_empty());
        assert_eq!(archive.disk_bytes, 0);
        assert_eq!(archive.retention_evicted, cursor);
        for index in 1..=3 {
            assert!(
                !archive.segment_path(index).exists(),
                "retention must close even the active file before removing it"
            );
        }
        archive.close(stamp(hour + RETENTION_NS)).unwrap();
    }

    #[test]
    fn disabled_is_inert_and_off_switch_stops_new_capture() {
        let disabled = Recorder::default();
        assert!(
            disabled
                .identity(IdentityDomain::Account, b"secret")
                .is_none()
        );
        let (_directory, recorder, mut archive) = setup();
        assert_eq!(
            recorder.record(stamp(1), Owner::Runtime, 1, None, event(&recorder)),
            RecordResult::Queued(1)
        );
        recorder.disable();
        assert_eq!(
            recorder.record(stamp(2), Owner::Runtime, 2, None, event(&recorder)),
            RecordResult::Disabled
        );
        archive.flush(stamp(2)).unwrap();
        assert_eq!(archive.persisted_through, Some(1));
    }
    #[test]
    fn pseudonyms_are_capture_local_domain_separated_and_raw_input_never_serializes() {
        let (_dir, recorder, mut archive) = setup();
        let a = recorder
            .identity(IdentityDomain::Account, b"synthetic-secret-email")
            .unwrap();
        assert_eq!(
            a,
            recorder
                .identity(IdentityDomain::Account, b"synthetic-secret-email")
                .unwrap()
        );
        assert_ne!(
            a,
            recorder
                .identity(IdentityDomain::Session, b"synthetic-secret-email")
                .unwrap()
        );
        let (_other, second, _) = setup();
        assert_ne!(
            a,
            second
                .identity(IdentityDomain::Account, b"synthetic-secret-email")
                .unwrap()
        );
        recorder.record(stamp(1), Owner::Runtime, 1, None, event(&recorder));
        archive.flush(stamp(1)).unwrap();
        let bytes = fs::read(archive.segment_path(1)).unwrap();
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains("synthetic-secret-email")
        );
    }
    #[test]
    fn queue_loss_and_gaps_are_explicit_and_clean_close_cannot_fabricate_initial_state() {
        let (_dir, recorder, archive) = setup();
        for n in 1..=QUEUE_RECORDS as u64 {
            assert_eq!(
                recorder.record(stamp(n), Owner::Runtime, n, None, event(&recorder)),
                RecordResult::Queued(n)
            );
        }
        assert_eq!(
            recorder.record(stamp(2000), Owner::Runtime, 2000, None, event(&recorder)),
            RecordResult::Rejected
        );
        let root = archive.root.clone();
        archive.close(stamp(2001)).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["loss"]["queue_full"], 1);
        assert_eq!(manifest["loss"]["owner_gap"], 1);
        assert_eq!(manifest["sealed"], true);
        for field in [
            "initial_state_reconciled",
            "owner_hooks_complete",
            "replay_ready",
            "actual_trace_accepted",
        ] {
            assert_eq!(manifest[field], false);
        }
    }
    #[test]
    fn utc_rollback_still_expires_current_epoch_by_known_monotonic_age() {
        let (_dir, recorder, mut archive) = setup();
        recorder.record(stamp(1), Owner::Runtime, 1, None, event(&recorder));
        archive.flush(stamp(1)).unwrap();
        archive
            .maintain(Stamp {
                utc_ns: (3 * RETENTION_NS) as i64,
                monotonic_ns: 2,
            })
            .unwrap();
        // A corrected UTC clock must still refuse root maintenance/capture.
        assert!(archive.maintain(stamp(RETENTION_NS)).is_err());
        assert!(!recorder.status().unwrap().enabled);
        assert!(
            archive.segment_path(1).exists(),
            "young local segment retained"
        );
        assert!(archive.maintain(stamp(RETENTION_NS + 1)).is_err());
        assert!(
            !archive.segment_path(1).exists(),
            "UTC rollback must not retain a provably expired local segment"
        );
        assert_eq!(archive.disk_bytes, 0);
        assert!(archive.close(stamp(RETENTION_NS + 2)).is_err());
    }
    #[test]
    fn retain_exactly_seven_days_and_never_evict_young_segments_for_space() {
        let (_dir, recorder, mut archive) = setup();
        recorder.record(stamp(1), Owner::Runtime, 1, None, event(&recorder));
        archive.flush(stamp(1)).unwrap();
        archive.maintain(stamp(RETENTION_NS)).unwrap();
        assert!(archive.segment_path(1).exists());
        archive.maintain(stamp(RETENTION_NS + 1)).unwrap();
        assert!(!archive.segment_path(1).exists());
        assert_eq!(archive.disk_bytes, 0);
        recorder.record(
            stamp(RETENTION_NS + 2),
            Owner::Runtime,
            2,
            None,
            event(&recorder),
        );
        archive.disk_bytes = DISK_BYTES - MANIFEST_RESERVE; // bounded-cap pressure fault.
        assert!(archive.flush(stamp(RETENTION_NS + 2)).is_err());
        assert_eq!(
            recorder
                .shared
                .as_ref()
                .unwrap()
                .state
                .lock()
                .unwrap()
                .loss
                .disk_cap,
            1
        );
    }
    #[test]
    fn invalid_values_clock_regression_and_sequence_exhaustion_are_not_zero_coverage() {
        let (_dir, recorder, mut archive) = setup();
        let account = recorder.identity(IdentityDomain::Account, b"a").unwrap();
        assert_eq!(
            recorder.record(
                stamp(10),
                Owner::Runtime,
                1,
                None,
                Event::Quota {
                    account,
                    window: Window::FiveHour,
                    revision: 1,
                    observed: stamp(10),
                    finite_percent_bits: Some(f64::NAN.to_bits()),
                    reset_utc_ns: None,
                    origin: Origin::IndependentPoll
                }
            ),
            RecordResult::Rejected
        );
        recorder.record(stamp(9), Owner::Runtime, 2, None, event(&recorder));
        recorder
            .shared
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .next_sequence = u64::MAX;
        assert_eq!(
            recorder.record(stamp(11), Owner::Runtime, 3, None, event(&recorder)),
            RecordResult::Rejected
        );
        archive.flush(stamp(11)).unwrap();
        let state = recorder.shared.as_ref().unwrap().state.lock().unwrap();
        assert_eq!(state.loss.invalid, 1);
        assert_eq!(state.loss.clock_regressed, 1);
        assert_eq!(state.loss.sequence_exhausted, 1);
    }

    #[test]
    fn write_failure_remains_unsealed_without_replay_claim() {
        let (_dir, recorder, mut archive) = setup();
        recorder.record(stamp(1), Owner::Runtime, 1, None, event(&recorder));
        fs::create_dir(archive.segment_path(1)).unwrap();
        assert!(archive.flush(stamp(1)).is_err());
        let state = recorder.shared.as_ref().unwrap().state.lock().unwrap();
        assert_eq!(state.loss.write_error, 1);
        assert_eq!(state.loss.unpersisted, 1);
        assert_eq!(state.kinds[EventKind::Account as usize].unpersisted, 1);
        assert_eq!(state.kinds[EventKind::Account as usize].persisted, 0);
        drop(state);
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(archive.root.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["sealed"], false);
        assert_eq!(manifest["crash_loss_count"], serde_json::Value::Null);
        assert_eq!(manifest["replay_ready"], false);
    }
    #[test]
    fn manifest_failure_stops_capture_and_cannot_be_retried_as_a_clean_close() {
        let (_dir, recorder, mut archive) = setup();
        fs::create_dir(archive.root.join("manifest.pending")).unwrap();
        recorder.record(stamp(1), Owner::Runtime, 1, None, event(&recorder));
        assert!(archive.flush(stamp(1)).is_err());
        assert!(archive.faulted);
        assert_eq!(
            recorder.record(stamp(2), Owner::Runtime, 2, None, event(&recorder)),
            RecordResult::Disabled
        );
        assert_eq!(
            recorder
                .shared
                .as_ref()
                .unwrap()
                .state
                .lock()
                .unwrap()
                .loss
                .write_error,
            1
        );
        assert!(archive.close(stamp(2)).is_err());
    }
    #[test]
    fn nanoseconds_survive_and_regressed_events_do_not_age_out_newer_bytes() {
        let (_dir, recorder, mut archive) = setup();
        recorder.record(stamp(10), Owner::Runtime, 1, None, event(&recorder));
        recorder.record(stamp(9), Owner::Runtime, 2, None, event(&recorder));
        archive.flush(stamp(10)).unwrap();
        let lines = fs::read_to_string(archive.segment_path(1)).unwrap();
        let records: Vec<serde_json::Value> = lines
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(records[0]["at"]["utc_ns"], 10);
        assert_eq!(records[1]["at"]["utc_ns"], 9);
        archive.maintain(stamp(RETENTION_NS + 9)).unwrap();
        assert!(archive.segment_path(1).exists());
        archive.maintain(stamp(RETENTION_NS + 10)).unwrap();
        assert!(!archive.segment_path(1).exists());
    }
    #[test]
    fn seven_day_segment_boundary_loss_is_explicit_not_complete_coverage() {
        let (_dir, recorder, mut archive) = setup();
        recorder.record(stamp(1), Owner::Runtime, 1, None, event(&recorder));
        recorder.record(
            stamp(HOUR_NS - 1),
            Owner::Runtime,
            2,
            None,
            event(&recorder),
        );
        archive.flush(stamp(HOUR_NS - 1)).unwrap();
        archive.maintain(stamp(RETENTION_NS + 1)).unwrap();
        assert!(!archive.segment_path(1).exists());
        assert_eq!(archive.retention_evicted, 2);
        assert_eq!(
            recorder
                .shared
                .as_ref()
                .unwrap()
                .state
                .lock()
                .unwrap()
                .loss
                .retention_boundary_segments,
            1
        );
    }
}
