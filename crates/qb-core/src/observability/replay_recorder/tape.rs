//! Schema 5 decision tape. Earlier roots are never rewritten.
//! Producers only enqueue bounded typed metadata. The sole writer compresses and syncs.
use super::*;
use crate::bindings::decision_tape::DecisionRecord;
use crate::bindings::permit_tape::{self, PermitRecord, QuotaKey, QuotaSnapshot};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, Read},
};

pub(super) const TAPE_SCHEMA: u16 = 5;
const RECORD_BYTES: usize = 256 * 1024;
const QUEUE_BYTES: usize = 8 * 1024 * 1024;
const HOUR_BYTES: u64 = 20_000_000;
const DECODED_SEGMENT_BYTES: usize = 16 * 1024 * 1024;
const GROUP_BYTES: usize = 1024 * 1024;
const MAX_ACCOUNTS: usize = 4096;
const MAX_RECORDS: usize = 1_000_000;
const MAX_QUOTA_SNAPSHOTS: usize = 16_384;
const MAX_QUOTA_DICTIONARY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub schema: u16,
    pub start: Stamp,
    pub capture: String,
    pub identity_domain_version: u16,
    pub build: BuildIdentity,
    pub provenance: Provenance,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Losses {
    pub capture_aborted: u64,
    pub budget: u64,
    pub queue_full: u64,
    pub invalid: u64,
    pub clock_regressed: u64,
    pub owner_gap: u64,
    pub producer_failure: u64,
    pub disk_cap: u64,
    pub write_error: u64,
    pub unpersisted: u64,
    pub saturated: bool,
}
impl Losses {
    fn total(self) -> u64 {
        [
            self.capture_aborted,
            self.budget,
            self.queue_full,
            self.invalid,
            self.clock_regressed,
            self.owner_gap,
            self.producer_failure,
            self.disk_cap,
            self.write_error,
            self.unpersisted,
            u64::from(self.saturated),
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentInfo {
    pub index: u64,
    pub records: u64,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub compressed_bytes: u64,
    pub encoded_bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u16,
    pub sealed: bool,
    pub capture: String,
    pub attempted: u64,
    pub accepted: u64,
    pub persisted: u64,
    pub deduplicated: u64,
    pub permit_records: u64,
    pub permit_not_comparable: u64,
    pub encoded_bytes: u64,
    pub compressed_bytes: u64,
    pub hourly_encoded_byte_limit: u64,
    pub budget_window_ns: u64,
    pub budget_window_used: u64,
    pub budget_window_stopped: bool,
    pub losses: Losses,
    pub segments: Vec<SegmentInfo>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "facts", deny_unknown_fields)]
pub enum Payload {
    DecisionTape(Box<DecisionRecord>),
    PermitTape(Box<PermitEnvelope>),
    PermitCommit { proposal: u64 },
    Metadata(Box<Event>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitEnvelope {
    pub record: PermitRecord,
    pub quotas: Vec<QuotaSnapshot>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub sequence: u64,
    pub at: Stamp,
    pub owner: Owner,
    pub owner_cursor: u64,
    pub cause: Option<u64>,
    pub event: Payload,
}
struct Queued {
    sequence: u64,
    bytes: Vec<u8>,
}
struct TapeState {
    enabled: bool,
    next: u64,
    last: Stamp,
    cursors: [Option<u64>; 8],
    window: u64,
    used: u64,
    budget_stopped: bool,
    queued_bytes: usize,
    queue: VecDeque<Queued>,
    accounts: BTreeMap<[u8; 32], [u8; 32]>,
    quotas: BTreeSet<QuotaKey>,
    quota_bytes: usize,
    manifest: Manifest,
}
struct TapeShared {
    key: [u8; 32],
    state: Mutex<TapeState>,
}
#[derive(Clone)]
pub(crate) struct Producer {
    shared: Arc<TapeShared>,
}
pub(crate) struct Writer {
    producer: Producer,
    store: EpochStore,
    root: PathBuf,
    active: Option<(fs::File, SegmentInfo, Sha256)>,
    segments: Vec<SegmentInfo>,
    faulted: bool,
    #[cfg(test)]
    fail_sync: bool,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn compact_id(id: Id) -> String {
    // All 256 HMAC bits retained for recorder-owned identities.
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id.0)
}
#[cfg(test)]
pub(crate) fn open(root: &Path, limits: Limits, at: Stamp) -> io::Result<(Producer, Writer)> {
    open_with_provenance(root, limits, at, Provenance::OperatorOptIn)
}

pub(crate) fn open_with_provenance(
    root: &Path,
    limits: Limits,
    at: Stamp,
    provenance: Provenance,
) -> io::Result<(Producer, Writer)> {
    let mut executable = fs::File::open(std::env::current_exe()?)?;
    let mut executable_hash = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = executable.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        executable_hash.update(&buffer[..read]);
    }
    let build = BuildIdentity {
        installed_executable: Some(Fingerprint(executable_hash.finalize().into())),
        ..BuildIdentity::default()
    };
    let (store, path) = EpochStore::open_schema(root, limits, at, TAPE_SCHEMA)?;
    let key: [u8; 32] = rand::random();
    let capture = compact_id(pseudonym(&key, IdentityDomain::Operation, b"capture"));
    // EpochStore creates only the common schema/start control. The richer header
    // is separate so its layout is not mistaken for an old EpochHeader.
    write_control(
        &path.join("header.json"),
        &Header {
            schema: TAPE_SCHEMA,
            start: at,
            capture: capture.clone(),
            identity_domain_version: ID_DOMAIN_VERSION,
            build,
            provenance,
        },
    )?;
    let manifest = Manifest {
        schema: TAPE_SCHEMA,
        sealed: false,
        capture: capture.clone(),
        attempted: 0,
        accepted: 0,
        persisted: 0,
        deduplicated: 0,
        permit_records: 0,
        permit_not_comparable: 0,
        encoded_bytes: 0,
        compressed_bytes: 0,
        hourly_encoded_byte_limit: HOUR_BYTES,
        budget_window_ns: 0,
        budget_window_used: 0,
        budget_window_stopped: false,
        losses: Losses::default(),
        segments: Vec::new(),
    };
    let producer = Producer {
        shared: Arc::new(TapeShared {
            key,
            state: Mutex::new(TapeState {
                enabled: true,
                next: 0,
                last: at,
                cursors: [None; 8],
                window: 0,
                used: 0,
                budget_stopped: false,
                queued_bytes: 0,
                queue: VecDeque::new(),
                accounts: BTreeMap::new(),
                quotas: BTreeSet::new(),
                quota_bytes: 0,
                manifest,
            }),
        }),
    };
    let writer = Writer {
        producer: producer.clone(),
        store,
        root: path,
        active: None,
        segments: Vec::new(),
        faulted: false,
        #[cfg(test)]
        fail_sync: false,
    };
    writer.checkpoint(false)?;
    Ok((producer, writer))
}
impl Producer {
    pub(crate) fn identity(&self, domain: IdentityDomain, raw: &[u8]) -> Option<Id> {
        if raw.is_empty() || raw.len() > 256 {
            self.invalid();
            return None;
        }
        Some(pseudonym(&self.shared.key, domain, raw))
    }
    fn invalid(&self) {
        let mut s = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let l = &mut s.manifest.losses;
        increment(&mut l.invalid, &mut l.saturated, 1);
    }
    pub(crate) fn failed(&self) {
        let mut s = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let l = &mut s.manifest.losses;
        increment(&mut l.producer_failure, &mut l.saturated, 1);
    }
    pub(crate) fn disable(&self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enabled = false;
    }
    pub(crate) fn metadata(
        &self,
        at: Stamp,
        owner: Owner,
        cursor: u64,
        cause: Option<u64>,
        event: Event,
    ) -> Option<u64> {
        if !event.valid() {
            self.invalid();
            return None;
        }
        self.enqueue(at, owner, cursor, cause, Payload::Metadata(Box::new(event)))
    }
    pub(crate) fn decision(&self, at: Stamp, cursor: u64, record: &DecisionRecord) -> Option<u64> {
        // Check size before cloning any caller-owned vectors. Only this concrete
        // DTO is accepted, never arbitrary Serialize, JSON, accounts or tokens.
        if serde_json::to_writer(BoundedSink::default(), record).is_err() {
            self.invalid();
            return None;
        }
        let mut record = record.clone();
        if !self.protect(&mut record) {
            self.invalid();
            return None;
        }
        self.enqueue(
            at,
            Owner::Bindings,
            cursor,
            None,
            Payload::DecisionTape(Box::new(record)),
        )
    }
    pub(crate) fn permit(
        &self,
        at: Stamp,
        cursor: u64,
        inputs: &crate::bindings::kernel::DecisionInputs,
        actual: &crate::bindings::kernel::PermitDecision,
    ) -> Option<u64> {
        let mapper = |kind, raw: &str| {
            let domain = match kind {
                permit_tape::IdKind::Account => IdentityDomain::Account,
                permit_tape::IdKind::Session => IdentityDomain::Session,
            };
            let id = pseudonym(&self.shared.key, domain, raw.as_bytes());
            id.0[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let mut record = match permit_tape::capture(inputs, actual, mapper) {
            Ok(record) => record,
            Err(reason) => match permit_tape::refused_record(actual, reason, mapper) {
                Ok(record) => record,
                Err(_) => {
                    self.invalid();
                    return None;
                }
            },
        };
        let not_comparable = record.refusal.is_some()
            || (inputs.from.as_deref().is_some_and(|from| from != inputs.to)
                && inputs.permit.is_none());
        let mut s = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let quotas = match record
            .inputs
            .as_mut()
            .map(|i| i.intern_quotas(&s.quotas))
            .transpose()
        {
            Ok(quotas) => quotas.unwrap_or_default(),
            Err(_) => {
                let l = &mut s.manifest.losses;
                increment(&mut l.invalid, &mut l.saturated, 1);
                return None;
            }
        };
        if s.quotas.len().saturating_add(quotas.len()) > MAX_QUOTA_SNAPSHOTS {
            let l = &mut s.manifest.losses;
            increment(&mut l.invalid, &mut l.saturated, 1);
            return None;
        }
        let quota_bytes = quotas.iter().try_fold(0usize, |total, snapshot| {
            let bytes = serde_json::to_vec(snapshot).ok()?;
            total.checked_add(bytes.len())
        });
        let Some(quota_bytes) = quota_bytes
            .filter(|bytes| s.quota_bytes.saturating_add(*bytes) <= MAX_QUOTA_DICTIONARY_BYTES)
        else {
            let l = &mut s.manifest.losses;
            increment(&mut l.invalid, &mut l.saturated, 1);
            return None;
        };
        let keys: Vec<_> = quotas.iter().map(QuotaSnapshot::key).collect();
        let sequence = self.enqueue_locked(
            &mut s,
            at,
            Owner::Bindings,
            cursor,
            None,
            Payload::PermitTape(Box::new(PermitEnvelope { record, quotas })),
        )?;
        // References become reusable only after the whole typed envelope entered
        // the sole writer queue. A failed publication cannot strand references.
        s.quotas.extend(keys);
        s.quota_bytes += quota_bytes;
        s.manifest.permit_records += 1;
        s.manifest.permit_not_comparable += u64::from(not_comparable);
        Some(sequence)
    }

    pub(crate) fn permit_commit(&self, at: Stamp, cursor: u64, proposal: u64) -> Option<u64> {
        self.enqueue(
            at,
            Owner::Bindings,
            cursor,
            Some(proposal),
            Payload::PermitCommit { proposal },
        )
    }
    fn protect(&self, r: &mut DecisionRecord) -> bool {
        if r.schema != 1 || !valid_refusal(r) {
            return false;
        }
        identities(r, |domain, value| {
            if value.is_empty() || value.len() > 256 {
                return false;
            }
            let id = pseudonym(&self.shared.key, domain, value.as_bytes());
            // Native DecisionRecord comparison keys are 128-bit hex strings.
            *value = id.0[..16].iter().map(|b| format!("{b:02x}")).collect();
            true
        })
    }
    fn enqueue(
        &self,
        at: Stamp,
        owner: Owner,
        cursor: u64,
        cause: Option<u64>,
        event: Payload,
    ) -> Option<u64> {
        let mut s = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        self.enqueue_locked(&mut s, at, owner, cursor, cause, event)
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue_locked(
        &self,
        s: &mut TapeState,
        at: Stamp,
        owner: Owner,
        cursor: u64,
        cause: Option<u64>,
        mut event: Payload,
    ) -> Option<u64> {
        if !s.enabled {
            return None;
        }
        s.manifest.attempted = s.manifest.attempted.saturating_add(1);
        if at.monotonic_ns < s.last.monotonic_ns || at.utc_ns < s.last.utc_ns {
            let l = &mut s.manifest.losses;
            increment(&mut l.clock_regressed, &mut l.saturated, 1);
            return None;
        }
        s.last = at;
        if s.cursors[owner as usize].is_some_and(|n| n.checked_add(1) != Some(cursor)) {
            let l = &mut s.manifest.losses;
            increment(&mut l.owner_gap, &mut l.saturated, 1);
        }
        s.cursors[owner as usize] = Some(cursor);
        let account = if let Payload::Metadata(event) = &event
            && let Event::AccountState { facts } = event.as_ref()
        {
            let hash: [u8; 32] = Sha256::digest(serde_json::to_vec(facts).ok()?).into();
            if s.accounts.get(&facts.account.0) == Some(&hash) {
                s.manifest.deduplicated = s.manifest.deduplicated.saturating_add(1);
                return None;
            }
            if s.accounts.len() >= MAX_ACCOUNTS && !s.accounts.contains_key(&facts.account.0) {
                let l = &mut s.manifest.losses;
                increment(&mut l.invalid, &mut l.saturated, 1);
                return None;
            }
            Some((facts.account.0, hash))
        } else {
            None
        };
        let window = at.monotonic_ns / HOUR_NS;
        if window != s.window {
            s.window = window;
            s.used = 0;
            s.budget_stopped = false;
        }
        if s.budget_stopped {
            let l = &mut s.manifest.losses;
            increment(&mut l.budget, &mut l.saturated, 1);
            return None;
        }
        let Some(sequence) = s.next.checked_add(1) else {
            let l = &mut s.manifest.losses;
            increment(&mut l.invalid, &mut l.saturated, 1);
            return None;
        };
        if cause.is_some_and(|n| n == 0 || n > s.next) {
            let l = &mut s.manifest.losses;
            increment(&mut l.invalid, &mut l.saturated, 1);
            return None;
        }
        if let Payload::DecisionTape(record) = &mut event {
            record.ordinal = sequence;
        }
        if let Payload::PermitTape(envelope) = &mut event {
            envelope.record.ordinal = sequence;
        }
        let record = Record {
            sequence,
            at,
            owner,
            owner_cursor: cursor,
            cause,
            event,
        };
        let mut bytes = Vec::new();
        if serde_json::to_writer(BoundedSink::new(&mut bytes), &record).is_err() {
            let l = &mut s.manifest.losses;
            increment(&mut l.invalid, &mut l.saturated, 1);
            return None;
        }
        bytes.push(b'\n');
        if s.used + bytes.len() as u64 > HOUR_BYTES {
            s.budget_stopped = true;
            let l = &mut s.manifest.losses;
            increment(&mut l.budget, &mut l.saturated, 1);
            return None;
        }
        if s.queued_bytes + bytes.len() > QUEUE_BYTES || s.queue.len() >= QUEUE_RECORDS {
            let l = &mut s.manifest.losses;
            increment(&mut l.queue_full, &mut l.saturated, 1);
            return None;
        }
        s.next = sequence;
        s.used += bytes.len() as u64;
        s.queued_bytes += bytes.len();
        s.manifest.accepted += 1;
        s.manifest.encoded_bytes += bytes.len() as u64;
        if let Some((id, hash)) = account {
            s.accounts.insert(id, hash);
        }
        if let Payload::DecisionTape(record) = &record.event
            && record.capture_aborted
        {
            let l = &mut s.manifest.losses;
            increment(&mut l.capture_aborted, &mut l.saturated, 1);
        }
        s.queue.push_back(Queued { sequence, bytes });
        Some(sequence)
    }
}
// Exhaustive patterns deliberately make additions to native DTOs a compile-time
// privacy review: a new string field must never silently bypass epoch re-keying.
fn identities(
    r: &mut DecisionRecord,
    mut f: impl FnMut(IdentityDomain, &mut String) -> bool,
) -> bool {
    use crate::bindings::decision_tape::{CandidateRead, Inputs, LaunchInputs, ReadState};
    use crate::observability::decision_comparison::Outcome;
    let DecisionRecord {
        schema: _,
        ordinal: _,
        inputs,
        state,
        answers: _,
        rebind: _,
        outcome,
        capture_aborted: _,
        read_state_complete: _,
    } = r;
    let Inputs {
        entry: _,
        at: _,
        provider: _,
        key,
        parent,
        model,
        pinned,
        request_bytes: _,
        hold_allowed: _,
        hold_timed_out: _,
        coverage,
        root_hint_present: _,
        launch_metadata_valid: _,
        launch,
    } = inputs;
    let Outcome {
        kind: _,
        account,
        until: _,
        refusal: _,
        moves: _,
    } = outcome;
    for (value, domain) in [
        (key, IdentityDomain::Session),
        (parent, IdentityDomain::Session),
        (model, IdentityDomain::Model),
        (pinned, IdentityDomain::Account),
        (account, IdentityDomain::Account),
    ] {
        if let Some(value) = value
            && !f(domain, value)
        {
            return false;
        }
    }
    if let Some(coverage) = coverage {
        for value in coverage {
            if !f(IdentityDomain::Account, value) {
                return false;
            }
        }
    }
    if let Some(LaunchInputs {
        resume,
        model_is_explicit: _,
        account_preference,
        allow_move_for_model: _,
        operation_present: _,
        admission_present: _,
        entitlements,
    }) = launch
    {
        if let Some(value) = resume
            && !f(IdentityDomain::Session, value)
        {
            return false;
        }
        if let Some(value) = account_preference
            && !f(IdentityDomain::Account, value)
        {
            return false;
        }
        for (account, models) in entitlements {
            if !f(IdentityDomain::Account, account) {
                return false;
            }
            for model in models {
                if !f(IdentityDomain::Model, model) {
                    return false;
                }
            }
        }
    }
    if let Some(ReadState {
        candidates,
        bound,
        hold_until: _,
        outside_unknown: _,
        capacity_available: _,
        unsupported: _,
    }) = state
    {
        if let Some(value) = bound
            && !f(IdentityDomain::Account, value)
        {
            return false;
        }
        for CandidateRead {
            account,
            order: _,
            provider_matches: _,
            eligible: _,
            drain: _,
            outside: _,
            gate: _,
            unknown: _,
            weekly: _,
            five: _,
            usage,
            active: _,
            reserved: _,
            threshold: _,
            pacing: _,
            capped: _,
            placed: _,
        } in candidates
        {
            if !f(IdentityDomain::Account, account)
                || usage.is_some_and(|v| !v.is_finite() || v < 0.0)
            {
                return false;
            }
        }
    }
    true
}
fn valid_refusal(record: &DecisionRecord) -> bool {
    use crate::observability::decision_comparison::OutcomeKind;
    let refusal = record.outcome.refusal.as_deref();
    if record.capture_aborted {
        return record.outcome.kind == OutcomeKind::Refusal && refusal == Some("capture_aborted");
    }
    match refusal {
        None => record.outcome.kind != OutcomeKind::Refusal,
        Some(reason) => {
            record.outcome.kind == OutcomeKind::Refusal
                && matches!(
                    reason,
                    "invalid_metadata"
                        | "anchor_conflict"
                        | "unknown_launch"
                        | "no_eligible_account"
                        | "entitlements_unknown"
                        | "model_unsupported"
                        | "consent_required"
                        | "busy"
                        | "capacity"
                        | "policy_blocked"
                        | "move_guard"
                        | "binding_changed"
                        | "retired"
                )
        }
    }
}

// Limits encoding allocation even for a huge malformed typed input. One byte
// reserved for the JSONL newline. A sink-only pass can reject before cloning.
#[derive(Default)]
struct BoundedSink<'a> {
    output: Option<&'a mut Vec<u8>>,
    written: usize,
}
impl<'a> BoundedSink<'a> {
    fn new(output: &'a mut Vec<u8>) -> Self {
        Self {
            output: Some(output),
            written: 0,
        }
    }
}
impl Write for BoundedSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() >= RECORD_BYTES.saturating_sub(self.written) {
            return Err(io::Error::other("decision_tape_record_size"));
        }
        self.written += bytes.len();
        if let Some(output) = &mut self.output {
            output.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Writer {
    fn checkpoint(&self, sealed: bool) -> io::Result<()> {
        let mut manifest = {
            let s = self
                .producer
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut m = s.manifest.clone();
            m.budget_window_ns = s.window * HOUR_NS;
            m.budget_window_used = s.used;
            m.budget_window_stopped = s.budget_stopped;
            m
        };
        manifest.sealed = sealed;
        manifest.segments = self.segments.clone();
        if let Some((_, segment, _)) = &self.active {
            manifest.segments.push(segment.clone());
        }
        let bytes = serde_json::to_vec(&manifest).map_err(io::Error::other)?;
        if bytes.len() as u64 > MANIFEST_RESERVE / 2 {
            return Err(io::Error::other("decision_tape_manifest_size"));
        }
        let path = self.root.join("manifest.pending");
        if path.exists() {
            regular_file(&path)?;
            fs::remove_file(&path)?;
        }
        write_control_large(&path, &bytes)?;
        fs::rename(path, self.root.join("manifest.json"))
    }
    fn seal_active(&mut self) {
        if let Some((file, segment, _)) = self.active.take() {
            drop(file);
            self.segments.push(segment);
        }
    }
    fn append(&mut self, batch: &[Queued]) -> io::Result<()> {
        let mut raw = Vec::new();
        for pending in batch {
            raw.extend_from_slice(&pending.bytes);
        }
        // Each group is a complete zstd frame. A synced prefix is independently
        // decodable; no unclosed streaming encoder is advertised as persisted.
        let compressed = zstd::stream::encode_all(raw.as_slice(), 3)?;
        if self.active.as_ref().is_some_and(|(_, s, _)| {
            s.compressed_bytes + compressed.len() as u64 > SEGMENT_BYTES
                || s.encoded_bytes + raw.len() as u64 > DECODED_SEGMENT_BYTES as u64
        }) {
            self.seal_active();
        }
        let persisted_bytes: u64 = self
            .segments
            .iter()
            .map(|s| s.compressed_bytes)
            .sum::<u64>()
            + self
                .active
                .as_ref()
                .map_or(0, |(_, s, _)| s.compressed_bytes);
        if persisted_bytes + self.store.other_bytes + STORE_RESERVE + compressed.len() as u64
            > self.store.limits.disk_bytes
            || self.active.is_none() && self.segments.len() >= MAX_SEGMENTS
        {
            let mut s = self
                .producer
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let l = &mut s.manifest.losses;
            increment(&mut l.disk_cap, &mut l.saturated, 1);
            return Err(io::Error::other("decision_tape_disk_cap"));
        }
        if self.active.is_none() {
            let index = self.segments.len() as u64;
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.root.join(segment_name(index)))?;
            self.active = Some((
                file,
                SegmentInfo {
                    index,
                    records: 0,
                    first_sequence: batch[0].sequence,
                    last_sequence: batch[0].sequence,
                    compressed_bytes: 0,
                    encoded_bytes: 0,
                    sha256: digest(&[]),
                },
                Sha256::new(),
            ));
        }
        let (file, segment, hash) = self.active.as_mut().expect("opened segment");
        file.write_all(&compressed)?;
        #[cfg(test)]
        if self.fail_sync {
            return Err(io::Error::other("synthetic_sync_failure"));
        }
        file.sync_data()?;
        // Commit receipts only after the complete group was synced.
        hash.update(&compressed);
        segment.sha256 = format!("{:x}", hash.clone().finalize());
        segment.compressed_bytes += compressed.len() as u64;
        segment.encoded_bytes += raw.len() as u64;
        segment.records += batch.len() as u64;
        segment.last_sequence = batch.last().expect("nonempty group").sequence;
        let mut s = self
            .producer
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        s.manifest.persisted += batch.len() as u64;
        s.manifest.compressed_bytes += compressed.len() as u64;
        Ok(())
    }
    pub(crate) fn flush(&mut self, now: Stamp) -> io::Result<()> {
        if self.faulted {
            return Err(io::Error::other("decision_tape_faulted"));
        }
        let result = self.flush_inner(now);
        if result.is_err() {
            self.faulted = true;
            let mut s = self
                .producer
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            s.enabled = false;
            let pending = s.manifest.accepted.saturating_sub(s.manifest.persisted);
            let l = &mut s.manifest.losses;
            increment(&mut l.write_error, &mut l.saturated, 1);
            increment(&mut l.unpersisted, &mut l.saturated, pending);
            s.queue.clear();
            s.queued_bytes = 0;
            drop(s);
            let _ = self.checkpoint(false);
        }
        result
    }
    fn flush_inner(&mut self, now: Stamp) -> io::Result<()> {
        self.store.maintain(now)?;
        let queue = {
            let mut s = self
                .producer
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            s.queued_bytes = 0;
            std::mem::take(&mut s.queue)
        };
        let batch: Vec<_> = queue.into_iter().collect();
        let mut start = 0;
        while start < batch.len() {
            let mut end = start + 1;
            let mut bytes = batch[start].bytes.len();
            while end < batch.len() && bytes + batch[end].bytes.len() <= GROUP_BYTES {
                bytes += batch[end].bytes.len();
                end += 1;
            }
            self.append(&batch[start..end])?;
            start = end;
        }
        self.checkpoint(false)
    }
    pub(crate) fn close(mut self, now: Stamp) -> io::Result<()> {
        self.producer.disable();
        self.flush(now)?;
        self.seal_active();
        self.checkpoint(true)
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.producer.disable();
    }
}
fn segment_name(index: u64) -> String {
    format!("s{index:016x}.jsonl.zst")
}
fn write_control_large(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
fn bounded_file(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    if regular_file(path)? > limit {
        return Err(io::Error::other("decision_tape_file_size"));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("decision_tape_file_size"));
    }
    Ok(bytes)
}

/// Bounded verification of the actual archived bytes. This is a decision-local
/// tape, not a full owner restore or an assertion of production trace coverage.
pub fn read_epoch(path: &Path) -> io::Result<crate::observability::decision_reader::Epoch> {
    let fail = || io::Error::other("decision_tape_insufficient_or_invalid");
    if !fs::symlink_metadata(path)?.file_type().is_dir() {
        return Err(fail());
    }
    let epoch_bytes = bounded_file(&path.join("epoch.json"), CONTROL_BYTES)?;
    let epoch: EpochHeader = serde_json::from_slice(&epoch_bytes).map_err(|_| fail())?;
    if epoch.schema != TAPE_SCHEMA {
        return Err(io::Error::other("decision_tape_unsupported_schema"));
    }
    let header_bytes = bounded_file(&path.join("header.json"), CONTROL_BYTES)?;
    let header: Header = serde_json::from_slice(&header_bytes).map_err(|_| fail())?;
    let manifest_path = path.join("manifest.json");
    let manifest_bytes = bounded_file(&manifest_path, MANIFEST_RESERVE / 2)?;
    let m: Manifest = serde_json::from_slice(&manifest_bytes).map_err(|_| fail())?;
    if header.schema != TAPE_SCHEMA
        || m.schema != TAPE_SCHEMA
        || header.capture != m.capture
        || header.start.utc_ns != epoch.start_utc_ns
        || header.identity_domain_version != ID_DOMAIN_VERSION
        || m.hourly_encoded_byte_limit != HOUR_BYTES
        || m.budget_window_used > HOUR_BYTES
        || m.segments.len() > MAX_SEGMENTS
        || m.persisted > MAX_RECORDS as u64
        || m.accepted < m.persisted
    {
        return Err(fail());
    }
    let mut hash = Sha256::new();
    hash.update(b"quota-baton/decision-tape-v5/epoch\0");
    for bytes in [&epoch_bytes, &header_bytes] {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    hash.update((manifest_bytes.len() as u64).to_be_bytes());
    hash.update(&manifest_bytes);
    let mut sequence = 0u64;
    let mut count = 0u64;
    let mut encoded = 0u64;
    let mut compressed = 0u64;
    let mut decisions = Vec::new();
    let mut permits = Vec::new();
    let mut quota_dictionary = BTreeMap::new();
    let mut quota_dictionary_bytes = 0usize;
    let mut allowed_proposals = BTreeSet::new();
    let mut committed_proposals = BTreeSet::new();
    let mut not_comparable = 0u64;
    let mut witness = Sha256::new();
    let mut witnesses = 0;
    let mut witness_before_decisions = true;
    let mut aborted = 0u64;
    let mut read_states_complete = true;
    let mut last_stamp = header.start;
    for (index, segment) in m.segments.iter().enumerate() {
        if segment.index != index as u64
            || segment.compressed_bytes > SEGMENT_BYTES
            || segment.encoded_bytes > DECODED_SEGMENT_BYTES as u64
            || segment.records == 0
            || count.saturating_add(segment.records) > MAX_RECORDS as u64
        {
            return Err(fail());
        }
        let file_path = path.join(segment_name(segment.index));
        if regular_file(&file_path)? != segment.compressed_bytes {
            return Err(fail());
        }
        let bytes = bounded_file(&file_path, SEGMENT_BYTES)?;
        if digest(&bytes) != segment.sha256 {
            return Err(fail());
        }
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(&bytes);
        let mut decoder = zstd::stream::read::Decoder::new(bytes.as_slice())?;
        decoder.window_log_max(24)?;
        let mut reader = io::BufReader::new(decoder.take(DECODED_SEGMENT_BYTES as u64 + 1));
        let mut segment_count = 0u64;
        let mut segment_encoded = 0u64;
        loop {
            let mut line = Vec::new();
            let read = reader
                .by_ref()
                .take(RECORD_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            segment_encoded += read as u64;
            if read > RECORD_BYTES
                || line.last() != Some(&b'\n')
                || segment_encoded > DECODED_SEGMENT_BYTES as u64
                || count >= MAX_RECORDS as u64
            {
                return Err(fail());
            }
            let record: Record = serde_json::from_slice(&line).map_err(|_| fail())?;
            if record.sequence != sequence.checked_add(1).ok_or_else(fail)?
                || record.at.monotonic_ns < last_stamp.monotonic_ns
                || record.at.utc_ns < last_stamp.utc_ns
                || record.cause.is_some_and(|n| n == 0 || n >= record.sequence)
            {
                return Err(fail());
            }
            if segment_count == 0 && segment.first_sequence != record.sequence {
                return Err(fail());
            }
            sequence = record.sequence;
            last_stamp = record.at;
            count += 1;
            segment_count += 1;
            match record.event {
                Payload::PermitTape(mut envelope) => {
                    if envelope.record.ordinal != sequence {
                        return Err(fail());
                    }
                    if witnesses == 0 {
                        witness_before_decisions = false;
                    }
                    let references: BTreeSet<_> = envelope
                        .record
                        .inputs
                        .as_ref()
                        .into_iter()
                        .flat_map(|inputs| inputs.accounts.values())
                        .filter_map(|candidate| match &candidate.quota {
                            permit_tape::QuotaInput::Ref { digest } => Some(QuotaKey {
                                account: candidate.account.id.clone(),
                                revision: candidate.revision,
                                digest: *digest,
                            }),
                            _ => None,
                        })
                        .collect();
                    for snapshot in envelope.quotas {
                        let key = snapshot.key();
                        if !references.contains(&key)
                            || quota_dictionary.contains_key(&key)
                            || quota_dictionary.len() >= MAX_QUOTA_SNAPSHOTS
                        {
                            return Err(fail());
                        }
                        quota_dictionary_bytes = quota_dictionary_bytes
                            .checked_add(serde_json::to_vec(&snapshot).map_err(|_| fail())?.len())
                            .ok_or_else(fail)?;
                        if quota_dictionary_bytes > MAX_QUOTA_DICTIONARY_BYTES {
                            return Err(fail());
                        }
                        quota_dictionary.insert(key, snapshot);
                    }
                    if let Some(inputs) = &mut envelope.record.inputs {
                        inputs
                            .resolve_quotas(&quota_dictionary)
                            .map_err(|_| fail())?;
                    }
                    permit_tape::validate_record(&envelope.record).map_err(|_| fail())?;
                    not_comparable += u64::from(
                        envelope.record.refusal.is_some()
                            || envelope.record.inputs.as_ref().is_some_and(|inputs| {
                                inputs.from.as_deref().is_some_and(|from| from != inputs.to)
                                    && inputs.permit.is_none()
                            }),
                    );
                    if matches!(
                        &envelope.record.actual,
                        crate::bindings::kernel::PermitDecision::Allowed(_)
                    ) {
                        allowed_proposals.insert(sequence);
                    }
                    permits.push(envelope.record);
                }
                Payload::PermitCommit { proposal } => {
                    if record.cause != Some(proposal)
                        || proposal >= sequence
                        || !allowed_proposals.contains(&proposal)
                        || !committed_proposals.insert(proposal)
                    {
                        return Err(fail());
                    }
                }
                Payload::DecisionTape(mut decision) => {
                    if !identities(&mut decision, |_, id| {
                        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
                    }) {
                        return Err(fail());
                    }
                    if decision.ordinal != sequence || !valid_refusal(&decision) {
                        return Err(fail());
                    }
                    aborted += u64::from(decision.capture_aborted);
                    read_states_complete &= decision.read_state_complete;
                    if witnesses == 0 {
                        witness_before_decisions = false;
                    }
                    decisions.push(*decision);
                }
                Payload::Metadata(event) if matches!(*event, Event::InitialWitness { .. }) => {
                    if !decisions.is_empty() || !permits.is_empty() {
                        witness_before_decisions = false;
                    }
                    let bytes = serde_json::to_vec(&event).map_err(|_| fail())?;
                    witness.update((bytes.len() as u64).to_be_bytes());
                    witness.update(bytes);
                    witnesses += 1;
                }
                Payload::Metadata(_) => {}
            }
        }
        if segment_count != segment.records
            || sequence != segment.last_sequence
            || segment_encoded != segment.encoded_bytes
        {
            return Err(fail());
        }
        compressed += segment.compressed_bytes;
        encoded += segment_encoded;
    }
    // Unknown or unlisted segments cannot be silently excluded from a verdict.
    for (n, entry) in fs::read_dir(path)?.enumerate() {
        if n >= MAX_SEGMENTS + 4 {
            return Err(fail());
        }
        let entry = entry?;
        let size = regular_file(&entry.path())?;
        let name = entry.file_name();
        if name == "manifest.pending" && (m.sealed || size > MANIFEST_RESERVE / 2) {
            return Err(fail());
        }
        if ![
            "epoch.json",
            "header.json",
            "manifest.json",
            "manifest.pending",
        ]
        .iter()
        .any(|s| name == *s)
            && !m
                .segments
                .iter()
                .any(|s| name == segment_name(s.index).as_str())
        {
            return Err(fail());
        }
    }
    if count != m.persisted
        || compressed != m.compressed_bytes
        || encoded > m.encoded_bytes
        || aborted > m.losses.capture_aborted
        || m.sealed && (m.accepted != m.persisted || aborted != m.losses.capture_aborted)
        || permits.len() as u64 != m.permit_records
        || not_comparable != m.permit_not_comparable
    {
        return Err(fail());
    }
    let losses = m.losses.total();
    let producer_build = header
        .build
        .installed_executable
        .map(|h| h.0.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let complete = m.sealed
        && losses == 0
        && m.accepted == m.persisted
        && producer_build.is_some()
        && read_states_complete
        && encoded == m.encoded_bytes
        && witnesses > 0
        && witness_before_decisions
        && !decisions.is_empty();
    let permit_complete = m.sealed
        && losses == 0
        && m.accepted == m.persisted
        && producer_build.is_some()
        && encoded == m.encoded_bytes
        && witnesses > 0
        && witness_before_decisions
        && !permits.is_empty();
    Ok(crate::observability::decision_reader::Epoch {
        producer_build,
        tape_sha256: format!("{:x}", hash.finalize()),
        initial_witness: if witnesses == 0 {
            String::new()
        } else {
            format!("{:x}", witness.finalize())
        },
        losses,
        complete,
        decisions,
        permits,
        permit_complete,
        permit_commits: committed_proposals.len() as u64,
        provenance: header.provenance,
        capture_started_utc_ns: header.start.utc_ns,
        capture_ended_utc_ns: last_stamp.utc_ns,
    })
}

#[cfg(test)]
mod tests;
