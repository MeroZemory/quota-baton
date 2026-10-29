//! Schema 8 storage only. Operates on already privacy-projected tape DTOs.
//! No native Account is accepted here; dictionaries live for exactly one epoch.
use super::*;
use crate::bindings::permit_tape::selection::SelectionRow;
use chrono::{DateTime, Utc};
use serde_json::Value;

const MAX_DEFINITIONS: usize = 16_384;
const MAX_DICTIONARY_BYTES: usize = 16 * 1024 * 1024;
const MAX_REFERENCES: usize = 16_384;
// Offline reader bound on cumulative canonical Record JSONL, independent of
// compact capture budgets. No allocation is reserved for this limit.
const MAX_EXPANDED_EPOCH_BYTES: usize = 1024 * 1024 * 1024;
const HASH_DOMAIN: &[u8] = b"quota-baton/decision-tape-v8/slice\0";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "domain", content = "facts", deny_unknown_fields)]
enum Slice {
    SelectionRow(Box<SelectionRow>),
    RecentTuple(RecentTuple),
    MetricConfig(Box<crate::config::RoutingConfig>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecentTuple {
    account: String,
    value: (DateTime<Utc>, String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    id: u32,
    digest: [u8; 32],
    slice: Slice,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    r: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRecord {
    record: Value,
    definitions: Vec<Definition>,
}

/// Parse every JSON map before Value can discard duplicate keys. This includes
/// references, definitions, and nested maps in otherwise untouched raw leaves.
struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> serde::de::Visitor<'de> for UniqueVisitor {
            type Value = UniqueValue;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON with unique object keys")
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Bool(value)))
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Number(value.into())))
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Number(value.into())))
            }

            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| UniqueValue(Value::Number(number)))
                    .ok_or_else(|| E::custom("decision_tape_v8_number"))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value.to_owned())))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value)))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueValue(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(UniqueValue(Value::Array(values)))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom("decision_tape_v8_duplicate_key"));
                    }
                    let UniqueValue(value) = map.next_value()?;
                    values.insert(key, value);
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}

fn invalid() -> io::Error {
    io::Error::other("decision_tape_v8_codec")
}

fn json_error(_: serde_json::Error) -> io::Error {
    invalid()
}

fn slice_hash(slice: &Slice) -> io::Result<[u8; 32]> {
    let mut hash = Sha256::new();
    hash.update(HASH_DOMAIN);
    hash.update(serde_json::to_vec(slice).map_err(json_error)?);
    Ok(hash.finalize().into())
}

fn definition_bytes(d: &Definition) -> io::Result<usize> {
    // Charge full serialized definitions, not just the leaf facts, to the cap.
    Ok(serde_json::to_vec(d).map_err(json_error)?.len())
}

#[derive(Default)]
pub(super) struct Encoder {
    keys: BTreeMap<[u8; 32], u32>,
    bytes: usize,
}

pub(super) struct Update {
    definitions: Vec<Definition>,
    bytes: usize,
}

pub(super) struct Prepared {
    pub(super) bytes: Vec<u8>,
    pub(super) update: Update,
}

impl Encoder {
    pub(super) fn prepare(&self, record: &Record) -> io::Result<Prepared> {
        // Bound the expanded projected record before allocating the JSON tree.
        serde_json::to_writer(BoundedSink::default(), record).map_err(json_error)?;
        let mut value = serde_json::to_value(record).map_err(json_error)?;
        let mut definitions = Vec::new();
        let mut pending = BTreeMap::new();
        let mut bytes = 0usize;
        visit(&mut value, |slot, location| {
            let slice = location.capture(slot)?;
            let digest = slice_hash(&slice)?;
            let id = match self.keys.get(&digest).or_else(|| pending.get(&digest)) {
                Some(id) => *id,
                None => {
                    let id = self.keys.len() + definitions.len();
                    if id >= MAX_DEFINITIONS {
                        return Err(invalid());
                    }
                    let d = Definition {
                        id: id as u32,
                        digest,
                        slice,
                    };
                    bytes = bytes
                        .checked_add(definition_bytes(&d)?)
                        .ok_or_else(invalid)?;
                    if self.bytes + bytes > MAX_DICTIONARY_BYTES {
                        return Err(invalid());
                    }
                    pending.insert(digest, d.id);
                    definitions.push(d);
                    id as u32
                }
            };
            *slot = serde_json::to_value(Reference { r: id }).map_err(json_error)?;
            Ok(())
        })?;
        let mut encoded = Vec::new();
        serde_json::to_writer(
            BoundedSink::wire(&mut encoded),
            &WireRecord {
                record: value,
                definitions: definitions.clone(),
            },
        )
        .map_err(json_error)?;
        encoded.push(b'\n');
        Ok(Prepared {
            bytes: encoded,
            update: Update { definitions, bytes },
        })
    }

    /// Caller holds the producer lock and has already accepted the entire wire
    /// envelope into its queue. A rejected prepare/budget/queue never gets here.
    pub(super) fn commit(&mut self, update: Update) {
        for d in update.definitions {
            debug_assert_eq!(d.id as usize, self.keys.len());
            self.keys.insert(d.digest, d.id);
        }
        self.bytes += update.bytes;
    }
}

#[derive(Default)]
pub(super) struct Decoder {
    entries: Vec<Definition>,
    keys: BTreeSet<[u8; 32]>,
    bytes: usize,
    expanded_bytes: usize,
}

impl Decoder {
    pub(super) fn decode(&mut self, line: &[u8]) -> io::Result<Record> {
        if line.len() > WIRE_RECORD_BYTES {
            return Err(invalid());
        }
        let UniqueValue(raw) = serde_json::from_slice(line).map_err(json_error)?;
        let mut wire: WireRecord = serde_json::from_value(raw.clone()).map_err(json_error)?;
        // Some existing DTOs have serde defaults. They must not silently erase
        // unknown definition fields or normalize tampered typed leaf values.
        if serde_json::to_value(&wire).map_err(json_error)? != raw {
            return Err(invalid());
        }
        if self.entries.len() + wire.definitions.len() > MAX_DEFINITIONS {
            return Err(invalid());
        }
        let mut bytes = 0usize;
        let mut pending = BTreeSet::new();
        for (offset, d) in wire.definitions.iter().enumerate() {
            if d.id as usize != self.entries.len() + offset
                || slice_hash(&d.slice)? != d.digest
                || self.keys.contains(&d.digest)
                || !pending.insert(d.digest)
            {
                return Err(invalid());
            }
            bytes = bytes
                .checked_add(definition_bytes(d)?)
                .ok_or_else(invalid)?;
            if self.bytes + bytes > MAX_DICTIONARY_BYTES {
                return Err(invalid());
            }
        }
        let mut expanded = serde_json::to_vec(&wire.record).map_err(json_error)?.len();
        let mut used = BTreeSet::new();
        visit(&mut wire.record, |slot, location| {
            let reference: Reference = serde_json::from_value(slot.clone()).map_err(json_error)?;
            let id = reference.r as usize;
            let d = if id < self.entries.len() {
                &self.entries[id]
            } else {
                wire.definitions
                    .get(id - self.entries.len())
                    .ok_or_else(invalid)?
            };
            // Type domain, map-account binding, and definition hash are checked
            // before a value can be copied into the expanded record.
            let restored = location.restore(&d.slice)?;
            let old = serde_json::to_vec(slot).map_err(json_error)?.len();
            let new = serde_json::to_vec(&restored).map_err(json_error)?.len();
            expanded = expanded
                .checked_sub(old)
                .and_then(|n| n.checked_add(new))
                .ok_or_else(invalid)?;
            if expanded >= RECORD_BYTES {
                return Err(invalid());
            }
            *slot = restored;
            used.insert(id);
            Ok(())
        })?;
        if wire
            .definitions
            .iter()
            .any(|d| !used.contains(&(d.id as usize)))
        {
            return Err(invalid());
        }
        let record: Record = serde_json::from_value(wire.record).map_err(json_error)?;
        let mut expanded_sink = BoundedSink::default();
        serde_json::to_writer(&mut expanded_sink, &record).map_err(json_error)?;
        let expanded_bytes = self
            .expanded_bytes
            .checked_add(expanded_sink.written)
            .and_then(|n| n.checked_add(1)) // Canonical JSONL newline.
            .filter(|n| *n <= MAX_EXPANDED_EPOCH_BYTES)
            .ok_or_else(invalid)?;
        // A malformed or over-budget record leaves every reader counter and
        // dictionary unchanged. read_epoch propagates failure for the epoch.
        self.bytes += bytes;
        self.keys.extend(pending);
        self.entries.extend(wire.definitions);
        self.expanded_bytes = expanded_bytes;
        Ok(record)
    }
}

enum Location<'a> {
    Row(Option<&'a str>),
    Recent(&'a str),
    Config,
}

impl Location<'_> {
    fn capture(&self, value: &Value) -> io::Result<Slice> {
        let slice = match self {
            Self::Row(_) => Slice::SelectionRow(Box::new(
                serde_json::from_value(value.clone()).map_err(json_error)?,
            )),
            Self::Recent(account) => Slice::RecentTuple(RecentTuple {
                account: (*account).to_owned(),
                value: serde_json::from_value(value.clone()).map_err(json_error)?,
            }),
            Self::Config => Slice::MetricConfig(Box::new(
                serde_json::from_value(value.clone()).map_err(json_error)?,
            )),
        };
        if self.restore(&slice)? != *value {
            return Err(invalid());
        }
        Ok(slice)
    }

    fn restore(&self, slice: &Slice) -> io::Result<Value> {
        match (self, slice) {
            (Self::Row(account), Slice::SelectionRow(row))
                if account.is_none_or(|id| id == row.account.id) =>
            {
                serde_json::to_value(row).map_err(json_error)
            }
            (Self::Recent(account), Slice::RecentTuple(tuple)) if *account == tuple.account => {
                serde_json::to_value(&tuple.value).map_err(json_error)
            }
            (Self::Config, Slice::MetricConfig(config)) => {
                serde_json::to_value(config).map_err(json_error)
            }
            _ => Err(invalid()),
        }
    }
}

fn object(value: &mut Value) -> io::Result<&mut serde_json::Map<String, Value>> {
    value.as_object_mut().ok_or_else(invalid)
}

fn field<'a>(value: &'a mut Value, name: &str) -> io::Result<&'a mut Value> {
    object(value)?.get_mut(name).ok_or_else(invalid)
}

fn apply(
    slot: &mut Value,
    location: Location<'_>,
    refs: &mut usize,
    f: &mut impl FnMut(&mut Value, Location<'_>) -> io::Result<()>,
) -> io::Result<()> {
    *refs += 1;
    if *refs > MAX_REFERENCES {
        return Err(invalid());
    }
    f(slot, location)
}

/// Exhaustive storage boundaries, never a recursive arbitrary-JSON compressor.
/// Arrays are visited in place; map keys, order and repeated tuples survive.
fn visit(
    value: &mut Value,
    mut f: impl FnMut(&mut Value, Location<'_>) -> io::Result<()>,
) -> io::Result<()> {
    let event = field(value, "event")?;
    if event.get("kind").and_then(Value::as_str) != Some("SelectionTape") {
        return Ok(());
    }
    let inputs = field(field(field(event, "facts")?, "record")?, "inputs")?;
    if inputs.is_null() {
        return Ok(());
    }
    let mut refs = 0usize;
    for (id, row) in object(field(inputs, "accounts")?)? {
        apply(row, Location::Row(Some(id)), &mut refs, &mut f)?;
    }
    let selection = field(inputs, "selection")?;
    for name in ["candidates", "source_accounts"] {
        for row in field(selection, name)?.as_array_mut().ok_or_else(invalid)? {
            apply(row, Location::Row(None), &mut refs, &mut f)?;
        }
    }
    let metrics = field(selection, "metrics")?;
    apply(
        field(metrics, "config")?,
        Location::Config,
        &mut refs,
        &mut f,
    )?;
    for (account, recent) in object(field(metrics, "recent")?)? {
        for tuple in recent.as_array_mut().ok_or_else(invalid)? {
            apply(tuple, Location::Recent(account), &mut refs, &mut f)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
