//! Existing grants only. The parent must authenticate the local takeover and
//! validate every anchor against its full binding snapshot before restoration.
//! Never accept these objects from launch clients, disk, or a general import API.

use super::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::io::{self, Write};
use zeroize::Zeroize;

pub const MAX_HANDOVER_BYTES: usize = 16 * 1024 * 1024;
const SCHEMA: u32 = 1;

pub(crate) type HandoverActivity = (Zeroizing<String>, DateTime<Utc>);

/// Live launch secrets: authenticated loopback admin transport only. Never
/// persist or log this object or its serialization. No Debug or Display.
///
/// ```compile_fail
/// fn requires_debug<T: std::fmt::Debug>() {}
/// requires_debug::<qb_core::launch_tokens::HandoverSnapshot>();
/// ```
/// ```compile_fail
/// fn requires_display<T: std::fmt::Display>() {}
/// requires_display::<qb_core::launch_tokens::HandoverSnapshot>();
/// ```
#[derive(Serialize)]
pub struct HandoverSnapshot {
    schema: u32,
    exported_at_utc: DateTime<Utc>,
    rows: Vec<Row>,
}

#[derive(Serialize)]
struct Row {
    token_sha256: [u8; 32],
    metadata: LaunchMetadata,
    box_scope: Option<Scope>,
    remaining_idle_nanos: u64,
}

#[derive(Serialize)]
struct Scope {
    name: String,
    epoch: u64,
}

// Each sensitive field owns a wiping guard as soon as the deserializer hands
// over its allocation. Derived *wire* visitors can therefore fail at any later
// field, duplicate key, or unknown key without bypassing cleanup.
struct SecretString<const MAX: usize>(Zeroizing<String>);

impl<'de, const MAX: usize> Deserialize<'de> for SecretString<MAX> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Text<const MAX: usize>;
        impl<'de, const MAX: usize> serde::de::Visitor<'de> for Text<MAX> {
            type Value = SecretString<MAX>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded launch metadata text")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                self.visit_string(value.to_owned())
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                let text = SecretString(Zeroizing::new(value));
                if text.0.len() > MAX {
                    return Err(E::custom("launch metadata text exceeds bound"));
                }
                Ok(text)
            }
        }
        d.deserialize_string(Text::<MAX>)
    }
}

impl<const MAX: usize> Drop for SecretString<MAX> {
    fn drop(&mut self) {
        self.0.zeroize();
        #[cfg(test)]
        record_wipe(self.0.is_empty());
    }
}

struct SecretDigest(Zeroizing<TokenDigest>);

impl<'de> Deserialize<'de> for SecretDigest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Bytes;
        impl<'de> serde::de::Visitor<'de> for Bytes {
            type Value = SecretDigest;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("exactly 32 digest bytes")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut digest = SecretDigest(Zeroizing::new([0; 32]));
                for byte in digest.0.iter_mut() {
                    *byte = seq
                        .next_element()?
                        .ok_or_else(|| serde::de::Error::custom("short launch digest"))?;
                }
                if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::custom("long launch digest"));
                }
                Ok(digest)
            }
        }
        d.deserialize_seq(Bytes)
    }
}

impl Drop for SecretDigest {
    fn drop(&mut self) {
        self.0.zeroize();
        #[cfg(test)]
        record_wipe(*self.0 == [0; 32]);
    }
}

#[derive(Default)]
struct SecretModels(BTreeSet<String>);

impl<'de> Deserialize<'de> for SecretModels {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Models;
        impl<'de> serde::de::Visitor<'de> for Models {
            type Value = SecretModels;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded advertised model identifiers")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut models = SecretModels::default();
                let mut bytes = 0usize;
                while let Some(mut model) =
                    seq.next_element::<SecretString<MAX_MODEL_ID_BYTES>>()?
                {
                    bytes += model.0.len();
                    if models.0.len() == MAX_ADVERTISED_MODELS
                        || bytes > MAX_ADVERTISED_BYTES
                        || model.0.is_empty()
                        || model.0.chars().any(|c| c.is_control() || c.is_whitespace())
                        || models.0.contains(model.0.as_str())
                    {
                        return Err(serde::de::Error::custom(
                            "invalid advertised model identifiers",
                        ));
                    }
                    models.0.insert(std::mem::take(&mut *model.0));
                }
                Ok(models)
            }
        }
        d.deserialize_seq(Models)
    }
}

fn wipe_models(models: &mut BTreeSet<String>) {
    for mut model in std::mem::take(models) {
        model.zeroize();
        #[cfg(test)]
        record_wipe(model.is_empty());
    }
}

impl Drop for SecretModels {
    fn drop(&mut self) {
        wipe_models(&mut self.0);
    }
}

struct SecretMetadata(LaunchMetadata);

impl<'de> Deserialize<'de> for SecretMetadata {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            binding_anchor_key: SecretString<MAX_ANCHOR_BYTES>,
            advertised_models: SecretModels,
        }
        let mut wire = Wire::deserialize(d)?;
        validate_anchor(&wire.binding_anchor_key.0).map_err(serde::de::Error::custom)?;
        Ok(Self(LaunchMetadata {
            binding_anchor_key: std::mem::take(&mut *wire.binding_anchor_key.0),
            advertised_models: std::mem::take(&mut wire.advertised_models.0),
        }))
    }
}

impl Drop for SecretMetadata {
    fn drop(&mut self) {
        self.0.binding_anchor_key.zeroize();
        wipe_models(&mut self.0.advertised_models);
        #[cfg(test)]
        record_wipe(self.0.binding_anchor_key.is_empty());
    }
}

impl<'de> Deserialize<'de> for Row {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            token_sha256: SecretDigest,
            metadata: SecretMetadata,
            #[serde(deserialize_with = "required_scope")]
            box_scope: Option<Scope>,
            remaining_idle_nanos: u64,
        }
        let mut wire = Wire::deserialize(d)?;
        Ok(Self {
            token_sha256: *wire.token_sha256.0,
            metadata: LaunchMetadata {
                binding_anchor_key: std::mem::take(&mut wire.metadata.0.binding_anchor_key),
                advertised_models: std::mem::take(&mut wire.metadata.0.advertised_models),
            },
            box_scope: wire.box_scope,
            remaining_idle_nanos: wire.remaining_idle_nanos,
        })
    }
}

impl<'de> Deserialize<'de> for Scope {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            name: SecretString<63>,
            epoch: u64,
        }
        let mut wire = Wire::deserialize(d)?;
        Ok(Self {
            name: std::mem::take(&mut *wire.name.0),
            epoch: wire.epoch,
        })
    }
}

#[cfg(test)]
thread_local! { static WIPED_FIELDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
fn record_wipe(wiped: bool) {
    assert!(wiped, "handover field must be wiped before disposal");
    WIPED_FIELDS.with(|count| count.set(count.get() + 1));
}

fn required_scope<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Scope>, D::Error> {
    Option::<Scope>::deserialize(d)
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.name.zeroize();
        self.epoch.zeroize();
        #[cfg(test)]
        record_wipe(self.name.is_empty() && self.epoch == 0);
    }
}

impl Zeroize for Row {
    fn zeroize(&mut self) {
        self.token_sha256.zeroize();
        self.metadata.binding_anchor_key.zeroize();
        wipe_models(&mut self.metadata.advertised_models);
        self.remaining_idle_nanos.zeroize();
        if let Some(scope) = &mut self.box_scope {
            scope.name.zeroize();
            scope.epoch.zeroize();
        }
    }
}

impl Drop for Row {
    fn drop(&mut self) {
        self.zeroize();
        #[cfg(test)]
        record_wipe(
            self.token_sha256 == [0; 32]
                && self.metadata.binding_anchor_key.is_empty()
                && self.metadata.advertised_models.is_empty(),
        );
    }
}

impl Drop for HandoverSnapshot {
    fn drop(&mut self) {
        // Row/Scope drops wipe owned strings and digests on success and refusal.
        self.rows.clear();
    }
}

struct Budget(usize);
impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("handover bound"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HandoverError {
    #[error("unsupported launch handover schema")]
    Schema,
    #[error("launch handover exceeds its bounds")]
    Bounds,
    #[error("invalid launch handover data")]
    Invalid,
    #[error("invalid launch handover timestamp")]
    Timestamp,
    #[error("duplicate launch handover digest")]
    Duplicate,
}

// Bound rows during decoding rather than first allocating an arbitrary Vec.
fn bounded_rows<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Row>, D::Error> {
    struct Rows;
    impl<'de> serde::de::Visitor<'de> for Rows {
        type Value = Vec<Row>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("at most 4096 launch grants")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<Row>, A::Error> {
            let mut rows = Vec::with_capacity(CAPACITY);
            while let Some(row) = seq.next_element()? {
                if rows.len() == CAPACITY {
                    return Err(serde::de::Error::custom("too many launch grants"));
                }
                rows.push(row);
            }
            Ok(rows)
        }
    }
    d.deserialize_seq(Rows)
}

impl<'de> Deserialize<'de> for HandoverSnapshot {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: u32,
            exported_at_utc: DateTime<Utc>,
            #[serde(deserialize_with = "bounded_rows")]
            rows: Vec<Row>,
        }
        let wire = Wire::deserialize(d)?;
        let snapshot = Self {
            schema: wire.schema,
            exported_at_utc: wire.exported_at_utc,
            rows: wire.rows,
        };
        snapshot.validate().map_err(serde::de::Error::custom)?;
        Ok(snapshot)
    }
}

impl HandoverSnapshot {
    /// The enclosing lifecycle decoder must also bound its entire input body.
    pub fn from_json(bytes: &[u8]) -> Result<Self, HandoverError> {
        if bytes.len() > MAX_HANDOVER_BYTES {
            return Err(HandoverError::Bounds);
        }
        serde_json::from_slice(bytes).map_err(|_| HandoverError::Invalid)
    }

    /// Transient authenticated loopback admin transport only; never persist or log.
    pub fn to_json(&self) -> Result<Zeroizing<Vec<u8>>, HandoverError> {
        let encoded_len = self.validate()?;
        // Avoid allocator growth leaving old copies of the serialized secrets.
        let mut bytes = Zeroizing::new(Vec::with_capacity(encoded_len));
        serde_json::to_writer(&mut *bytes, self).map_err(|_| HandoverError::Invalid)?;
        Ok(bytes)
    }

    /// Parent checks these against the authoritative full binding snapshot.
    pub fn binding_anchor_keys(&self) -> impl Iterator<Item = &str> {
        self.rows
            .iter()
            .map(|row| row.metadata.binding_anchor_key.as_str())
    }

    /// Authenticated source activity implied by the already bounded remaining
    /// idle lease. This does not reset expiry to the successor's wall clock.
    pub(crate) fn binding_activity(&self) -> Result<Vec<HandoverActivity>, HandoverError> {
        self.validate()?;
        let mut activity = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            activity.push((
                Zeroizing::new(row.metadata.binding_anchor_key.clone()),
                source_activity(self.exported_at_utc, row.remaining_idle_nanos)?,
            ));
        }
        Ok(activity)
    }

    fn validate(&self) -> Result<usize, HandoverError> {
        if self.schema != SCHEMA {
            return Err(HandoverError::Schema);
        }
        if self.rows.len() > CAPACITY {
            return Err(HandoverError::Bounds);
        }
        if self.exported_at_utc.timestamp() < 0 {
            return Err(HandoverError::Timestamp);
        }
        let mut digests = Zeroizing::new(Vec::with_capacity(self.rows.len()));
        for row in &self.rows {
            digests.push(row.token_sha256);
            validate_anchor(&row.metadata.binding_anchor_key)
                .map_err(|_| HandoverError::Invalid)?;
            validate_models(&row.metadata.advertised_models).map_err(|_| HandoverError::Invalid)?;
            if row.remaining_idle_nanos == 0
                || u128::from(row.remaining_idle_nanos) > IDLE_TIMEOUT.as_nanos()
            {
                return Err(HandoverError::Invalid);
            }
            source_activity(self.exported_at_utc, row.remaining_idle_nanos)?;
            if let Some(scope) = &row.box_scope {
                // Validate without allocating a non-wiping BoxName copy.
                let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
                if scope.name.len() > 63
                    || !scope.name.bytes().next().is_some_and(alnum)
                    || !scope
                        .name
                        .bytes()
                        .all(|b| alnum(b) || matches!(b, b'-' | b'_'))
                {
                    return Err(HandoverError::Invalid);
                }
            }
        }
        digests.sort_unstable();
        if digests.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(HandoverError::Duplicate);
        }
        // Count without allocating a second copy of up to 16 MiB of metadata.
        let mut budget = Budget(MAX_HANDOVER_BYTES);
        serde_json::to_writer(&mut budget, self).map_err(|_| HandoverError::Bounds)?;
        Ok(MAX_HANDOVER_BYTES - budget.0)
    }
}

fn source_activity(
    exported_at: DateTime<Utc>,
    remaining_nanos: u64,
) -> Result<DateTime<Utc>, HandoverError> {
    let age = IDLE_TIMEOUT
        .checked_sub(Duration::from_nanos(remaining_nanos))
        .ok_or(HandoverError::Timestamp)?;
    let age = chrono::Duration::from_std(age).map_err(|_| HandoverError::Timestamp)?;
    let at = exported_at
        .checked_sub_signed(age)
        .ok_or(HandoverError::Timestamp)?;
    if at.timestamp() < 0 {
        return Err(HandoverError::Timestamp);
    }
    Ok(at)
}

struct GrantPlan {
    row: usize,
    scope: Option<usize>,
    last_activity: Instant,
    source_activity_millis: i64,
}

/// Prepared takeover grants stay in the wiping snapshot until binding import
/// succeeds. No registry keys or metadata allocations are transferred early.
/// Dropping this value is sufficient to discard an unsuccessful preparation.
pub(crate) struct PreparedHandover<'a> {
    snapshot: HandoverSnapshot,
    scopes: &'a [BoxScope],
    grants: Vec<GrantPlan>,
    clock: Arc<Clock>,
}

impl PreparedHandover<'_> {
    /// Call only after successful full binding import, before listeners.
    pub(crate) fn commit(mut self) -> LaunchTokenRegistry {
        let registry = LaunchTokenRegistry::with_clock(self.clock.clone());
        // Allocate once before inserting any digest or metadata. No populated
        // staging HashMap exists on the fallible binding-import path.
        let mut entries = HashMap::with_capacity(self.grants.len());
        for plan in &self.grants {
            let row = &mut self.snapshot.rows[plan.row];
            entries.insert(
                row.token_sha256,
                Entry {
                    metadata: LaunchMetadata {
                        binding_anchor_key: std::mem::take(&mut row.metadata.binding_anchor_key),
                        advertised_models: std::mem::take(&mut row.metadata.advertised_models),
                    },
                    box_scope: plan.scope.map(|index| self.scopes[index].clone()),
                    last_activity: plan.last_activity,
                    pins: 0,
                    deferred_owner: Weak::new(),
                    binding_activity: Arc::new(AtomicI64::new(plan.source_activity_millis)),
                },
            );
        }
        *registry.0.lock() = entries;
        // Snapshot still owns every inline digest, and wipes it on this return.
        registry
    }
}

impl LaunchTokenRegistry {
    pub fn export_handover(&self) -> Result<HandoverSnapshot, HandoverError> {
        self.export_handover_at(Utc::now())
    }

    fn export_handover_at(
        &self,
        exported_at_utc: DateTime<Utc>,
    ) -> Result<HandoverSnapshot, HandoverError> {
        let entries = self.0.lock();
        let now = (self.0.clock)();
        let mut rows = Vec::with_capacity(CAPACITY);
        let mut budget = Budget(MAX_HANDOVER_BYTES);
        for (digest, entry) in entries.iter() {
            let active = entry.pins != 0
                || entry
                    .deferred_owner
                    .upgrade()
                    .is_some_and(|owner| owner.live.load(Ordering::Acquire));
            if !active && now.saturating_duration_since(entry.last_activity) >= IDLE_TIMEOUT {
                continue;
            }
            let remaining = if active {
                // Runtime ownership cannot cross processes. Convert its liveness
                // exemption into one bounded idle grace, never pins or receipts.
                IDLE_TIMEOUT
            } else {
                IDLE_TIMEOUT.saturating_sub(now.saturating_duration_since(entry.last_activity))
            };
            if entry.last_activity > now {
                return Err(HandoverError::Timestamp);
            }
            let row = Row {
                token_sha256: *digest,
                metadata: entry.metadata.clone(),
                box_scope: entry.box_scope.as_ref().map(|scope| Scope {
                    name: scope.name().as_str().to_owned(),
                    epoch: scope.epoch(),
                }),
                remaining_idle_nanos: remaining.as_nanos() as u64,
            };
            serde_json::to_writer(&mut budget, &row).map_err(|_| HandoverError::Bounds)?;
            rows.push(row);
        }
        drop(entries);
        let snapshot = HandoverSnapshot {
            schema: SCHEMA,
            exported_at_utc,
            rows,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Startup constructor, before listeners. ONLY the trusted authenticated
    /// local takeover parent may call this, after validating the full bindings.
    /// `scopes` includes revoked epochs too; live revocation remains an ingress
    /// gate. Unknown/old epochs are omitted, never converted to unscoped grants.
    #[cfg(test)]
    pub(crate) fn restore_handover(
        snapshot: HandoverSnapshot,
        scopes: &[BoxScope],
    ) -> Result<Self, HandoverError> {
        Self::prepare_handover(snapshot, scopes).map(PreparedHandover::commit)
    }

    /// Trusted startup only. Commit after full binding import succeeds; drop on
    /// refusal. The borrowed exact registry scopes must stay alive until commit.
    pub(crate) fn prepare_handover<'a>(
        snapshot: HandoverSnapshot,
        scopes: &'a [BoxScope],
    ) -> Result<crate::launch_tokens::PreparedHandover<'a>, HandoverError> {
        Self::prepare_handover_at(snapshot, scopes, Utc::now(), Arc::new(Instant::now))
    }

    #[cfg(test)]
    fn restore_handover_at(
        snapshot: HandoverSnapshot,
        scopes: &[BoxScope],
        utc_now: DateTime<Utc>,
        clock: Arc<Clock>,
    ) -> Result<Self, HandoverError> {
        Self::prepare_handover_at(snapshot, scopes, utc_now, clock).map(PreparedHandover::commit)
    }

    fn prepare_handover_at<'a>(
        snapshot: HandoverSnapshot,
        scopes: &'a [BoxScope],
        utc_now: DateTime<Utc>,
        clock: Arc<Clock>,
    ) -> Result<PreparedHandover<'a>, HandoverError> {
        // Pair the supplied UTC sample with a monotonic sample before validation
        // work. Validation time then consumes TTL rather than extending it.
        let now = clock();
        snapshot.validate()?;
        if utc_now.timestamp() < 0 {
            return Err(HandoverError::Timestamp);
        }
        let downtime = utc_now
            .signed_duration_since(snapshot.exported_at_utc)
            .to_std()
            .map_err(|_| HandoverError::Timestamp)?;
        let idle_origin = now
            .checked_sub(IDLE_TIMEOUT)
            .ok_or(HandoverError::Timestamp)?;
        let mut grants = Vec::with_capacity(CAPACITY);
        for (index, row) in snapshot.rows.iter().enumerate() {
            let Some(remaining) =
                Duration::from_nanos(row.remaining_idle_nanos).checked_sub(downtime)
            else {
                continue;
            };
            if remaining.is_zero() {
                continue;
            }
            let scope = match row.box_scope.as_ref() {
                None => None,
                Some(scope) => {
                    let Some(actual) = scopes.iter().position(|actual| {
                        actual.name().as_str() == scope.name && actual.epoch() == scope.epoch
                    }) else {
                        continue;
                    };
                    Some(actual)
                }
            };
            grants.push(GrantPlan {
                row: index,
                scope,
                last_activity: idle_origin + remaining,
                source_activity_millis: source_activity(
                    snapshot.exported_at_utc,
                    row.remaining_idle_nanos,
                )?
                .timestamp_millis(),
            });
        }
        Ok(PreparedHandover {
            snapshot,
            scopes,
            grants,
            clock,
        })
    }

    /// After full binding import/anchor validation, before listeners. Do not
    /// hold the token lock while entering the binding table.
    pub(crate) fn reattach_binding_activity(&self, table: &Arc<crate::bindings::BindingTable>) {
        let activities = {
            let entries = self.0.lock();
            let mut activities = Vec::with_capacity(entries.len());
            for entry in entries.values() {
                activities.push((
                    Zeroizing::new(entry.metadata.binding_anchor_key.clone()),
                    entry.binding_activity.clone(),
                ));
            }
            activities
        };
        for (anchor, activity) in activities {
            table.bind_launch_activity(&anchor, &activity);
        }
    }
}

#[cfg(test)]
mod tests;
