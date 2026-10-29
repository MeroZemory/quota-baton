use super::*;
use crate::bindings::permit_tape::{AccountFacts, Field, HealthFacts};
use serde_json::json;

fn sample() -> Record {
    let row = SelectionRow {
        account: AccountFacts {
            id: "a:00000000000000000000000000000001".into(),
            generation: 7,
            provider: crate::accounts::Provider::Codex,
            plan: None,
            user_state: crate::accounts::UserState::Active,
            health: HealthFacts::Ready,
            auth_failures: 0,
            payment_failures: 0,
            payment_until: None,
            auth_probe_pending: false,
            quota_cooling: None,
            quota_credit_releasable: false,
            drain: false,
            allow_credits: false,
        },
        quota: Field::Missing,
        inflight: 3,
    };
    let id = row.account.id.clone();
    let first = json!([
        "2026-10-30T00:00:00Z",
        "codex:00000000000000000000000000000001"
    ]);
    let second = json!([
        "2026-10-30T00:00:01Z",
        "codex:00000000000000000000000000000002"
    ]);
    let inputs = json!({
        "now":"2026-10-30T00:01:00Z", "model":null, "key":null,
        "accounts":{(id.clone()):row}, "traffic":{}, "pending":{}, "groups":{},
        "bindings":{}, "accepted_unbound":[], "sticky":{}, "outside_failure":false,
        "selection":{
            "entry":"ordered", "provider":crate::accounts::Provider::Codex,
            "parent":null, "pinned":null, "hold_allowed":false, "preference":null,
            "root_hint":null, "candidates":[row.clone(),row.clone()], "source_accounts":[row],
            "account_order":[id.clone()],
            "affinity":{"bindings":{},"groups":{},"binding_count":0,"lru":[],"active":[],"pins":[],"launch_retention":{},"launch_anchors":{},"session_binding_max_secs":3600},
            "metrics":{"config":crate::config::RoutingConfig::default(),"recent":{(id.clone()):[first.clone(),second,first]},"idle_placements":[],"thresholds":{},"overload_marks":{},"pacing":{},"placements":{}},
            "policy":{
                "scope":{"key":null,"exact":"","legacy":null},"defaults":crate::routing::Settings::default(),
                "limits":crate::routing::ChurnLimits::default(),"revision":1,
                "state":{"sessions":{},"legacy_codex_roots":[]},"witness":{"sessions":{},"legacy_codex_roots":[]},
                "frozen":false,"history_overflow_until":null,"recovery_blocked":false,"lifecycle_recovery":false,
                "pool_moves":{},"target_pools":{}
            },
            "launches":{},"launch_aliases":[],"unknown_cost":[],"last_request_bytes":{},
            "outside_unknown":false,"outside_used":[],"entitled_targets":null,"return_guard":null
        }
    });
    Record {
        sequence: 1,
        at: Stamp {
            utc_ns: 1_000_001,
            monotonic_ns: 1,
        },
        owner: Owner::Bindings,
        owner_cursor: 1,
        cause: None,
        event: Payload::SelectionTape(Box::new(SelectionEnvelope {
            capture_id: 1,
            entry: crate::bindings::decision_tape::Entry::Chooser,
            invocation: 1,
            record: permit_tape::selection::SelectionRecord {
                schema: 1,
                ordinal: 1,
                inputs: Some(serde_json::from_value(inputs).unwrap()),
                actual: crate::bindings::kernel::selection::SelectionDecision {
                    choice: crate::bindings::kernel::selection::SelectionChoice::None,
                    label: crate::bindings::kernel::selection::SelectionLabel::New,
                    rebalance: None,
                    sole_target: None,
                    budget_source: None,
                },
                refusal: None,
            },
        })),
    }
}

fn round_trip(encoder: &mut Encoder, decoder: &mut Decoder, record: &Record) -> usize {
    let Prepared { bytes, update } = encoder.prepare(record).unwrap();
    let restored = decoder.decode(&bytes).unwrap();
    assert!(
        serde_json::to_value(record).unwrap() == serde_json::to_value(&restored).unwrap(),
        "canonical record changed"
    );
    encoder.commit(update);
    bytes.len()
}

#[test]
fn schema8_preserves_quota_states_revisions_identity_and_ordered_duplicate_tuples() {
    let mut encoder = Encoder::default();
    let mut decoder = Decoder::default();
    let mut record = sample();
    let first = round_trip(&mut encoder, &mut decoder, &record);
    assert_eq!(encoder.keys.len(), 4); // One shared row, config, two distinct tuples.
    let repeated = round_trip(&mut encoder, &mut decoder, &record);
    assert!(repeated < first);
    let prior = encoder.keys.len();
    for state in ["null", "missing"] {
        if let Payload::SelectionTape(s) = &mut record.event {
            let row = s
                .record
                .inputs
                .as_mut()
                .unwrap()
                .accounts
                .values_mut()
                .next()
                .unwrap();
            row.quota = if state == "null" {
                Field::Null
            } else {
                Field::Missing
            };
        }
        round_trip(&mut encoder, &mut decoder, &record);
    }
    assert_eq!(encoder.keys.len(), prior + 1);
    if let Payload::SelectionTape(s) = &mut record.event {
        let row = s
            .record
            .inputs
            .as_mut()
            .unwrap()
            .accounts
            .values_mut()
            .next()
            .unwrap();
        row.quota = Field::Value(permit_tape::QuotaFacts {
            windows: Field::Null,
            credits: Field::Missing,
            paid_serving_observed_at: Field::Missing,
        });
    }
    round_trip(&mut encoder, &mut decoder, &record);
    if let Payload::SelectionTape(s) = &mut record.event {
        s.record
            .inputs
            .as_mut()
            .unwrap()
            .accounts
            .values_mut()
            .next()
            .unwrap()
            .account
            .generation += 1;
    }
    round_trip(&mut encoder, &mut decoder, &record);
    assert_eq!(encoder.keys.len(), prior + 3);
}

fn changed_wire(change: impl FnOnce(&mut WireRecord)) -> Vec<u8> {
    let bytes = Encoder::default().prepare(&sample()).unwrap().bytes;
    let mut wire: WireRecord = serde_json::from_slice(&bytes).unwrap();
    change(&mut wire);
    serde_json::to_vec(&wire).unwrap()
}

#[test]
fn schema8_rejects_unknown_forward_hash_order_domain_and_account_tampering_atomically() {
    let cases: Vec<Vec<u8>> = vec![
        changed_wire(|w| w.definitions[0].digest[0] ^= 1),
        changed_wire(|w| w.definitions.swap(0, 1)),
        changed_wire(|w| w.definitions[0].id = 1),
        changed_wire(|w| {
            w.definitions.pop();
        }),
        changed_wire(|w| {
            *w.record["event"]["facts"]["record"]["inputs"]["accounts"]
                .as_object_mut()
                .unwrap()
                .values_mut()
                .next()
                .unwrap() = json!({"r":u32::MAX});
        }),
        changed_wire(|w| {
            let row = w.record["event"]["facts"]["record"]["inputs"]["accounts"]
                .as_object_mut()
                .unwrap()
                .values_mut()
                .next()
                .unwrap();
            *row = json!({"r":1}); // Config cannot stand in for a row.
        }),
        changed_wire(|w| {
            let Slice::SelectionRow(row) = &mut w.definitions[0].slice else {
                panic!("row")
            };
            row.account.id.push('0');
            w.definitions[0].digest = slice_hash(&w.definitions[0].slice).unwrap();
        }),
        changed_wire(|w| {
            let d = w
                .definitions
                .iter_mut()
                .find(|d| matches!(d.slice, Slice::RecentTuple(_)))
                .unwrap();
            if let Slice::RecentTuple(t) = &mut d.slice {
                t.account.push('0');
            }
            d.digest = slice_hash(&d.slice).unwrap();
        }),
    ];
    for bytes in cases {
        let mut decoder = Decoder::default();
        assert!(decoder.decode(&bytes).is_err());
        assert!(decoder.entries.is_empty() && decoder.keys.is_empty() && decoder.bytes == 0);
    }
    let bytes = Encoder::default().prepare(&sample()).unwrap().bytes;
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    value["definitions"][1]["slice"]["facts"]["unknown_config_field"] = json!(true);
    assert!(
        Decoder::default()
            .decode(&serde_json::to_vec(&value).unwrap())
            .is_err()
    );
    let mut encoder = Encoder::default();
    let Prepared { update, .. } = encoder.prepare(&sample()).unwrap();
    encoder.commit(update);
    let without_definitions = encoder.prepare(&sample()).unwrap().bytes;
    assert!(
        Decoder::default().decode(&without_definitions).is_err(),
        "new epoch must not inherit dictionary IDs"
    );
}

#[test]
fn schema8_late_record_failure_and_unused_definitions_leave_reader_state_unchanged() {
    for bytes in [
        changed_wire(|w| w.record["owner"] = Value::Null),
        changed_wire(|w| w.record["unknown_record_field"] = json!(true)),
        changed_wire(|w| {
            let mut d = w.definitions[0].clone();
            d.id = w.definitions.len() as u32;
            let Slice::SelectionRow(row) = &mut d.slice else {
                panic!("row")
            };
            row.inflight += 1;
            d.digest = slice_hash(&d.slice).unwrap();
            w.definitions.push(d);
        }),
    ] {
        let mut decoder = Decoder::default();
        assert!(decoder.decode(&bytes).is_err());
        assert!(decoder.entries.is_empty() && decoder.bytes == 0 && decoder.keys.is_empty());
        let valid = Encoder::default().prepare(&sample()).unwrap().bytes;
        assert!(
            decoder.decode(&valid).is_ok(),
            "late failure must not reserve IDs"
        );
    }
}

#[test]
fn schema8_rejects_encoded_duplicate_keys_in_refs_definitions_and_nested_maps() {
    let valid = String::from_utf8(Encoder::default().prepare(&sample()).unwrap().bytes).unwrap();
    let canonical: Value = serde_json::from_str(&valid).unwrap();
    for (original, duplicate) in [
        (r#"{"r":0}"#, r#"{"r":4294967295,"r":0}"#),
        (r#"{"r":0}"#, r#"{"r":4294967295,"\u0072":0}"#),
        (r#""id":0"#, r#""id":4294967295,"id":0"#),
        (r#""digest":["#, r#""digest":[],"digest":["#),
        (
            r#""domain":"SelectionRow""#,
            r#""domain":"Unknown","domain":"SelectionRow""#,
        ),
        (r#""inflight":3"#, r#""inflight":4,"inflight":3"#),
        (
            r#""binding_count":0"#,
            r#""binding_count":1,"binding_count":0"#,
        ),
        (r#""definitions":["#, r#""definitions":[],"definitions":["#),
    ] {
        let hostile = valid.replacen(original, duplicate, 1);
        assert!(hostile != valid, "test must modify encoded JSON");
        // Show this is valid JSON that ordinary Value parsing silently turns
        // back into the accepted record, rather than a syntax-error test.
        assert!(serde_json::from_str::<Value>(&hostile).unwrap() == canonical);
        let mut decoder = Decoder::default();
        assert!(decoder.decode(hostile.as_bytes()).is_err());
        assert!(decoder.entries.is_empty() && decoder.keys.is_empty());
        assert_eq!(decoder.bytes, 0);
        assert_eq!(decoder.expanded_bytes, 0);
        assert!(decoder.decode(valid.as_bytes()).is_ok());
    }
}

#[test]
fn schema8_epoch_expansion_accepts_exact_limit_then_rejects_without_committing() {
    let record = sample();
    let canonical_bytes = serde_json::to_vec(&record).unwrap().len() + 1;
    let mut encoder = Encoder::default();
    let Prepared {
        bytes: first,
        update,
    } = encoder.prepare(&record).unwrap();
    encoder.commit(update);
    let compact = encoder.prepare(&record).unwrap().bytes;
    let mut decoder = Decoder {
        expanded_bytes: MAX_EXPANDED_EPOCH_BYTES - canonical_bytes,
        ..Decoder::default()
    };
    assert!(decoder.decode(&first).is_ok());
    assert_eq!(decoder.expanded_bytes, MAX_EXPANDED_EPOCH_BYTES);
    let entries = serde_json::to_vec(&decoder.entries).unwrap();
    let hashes = decoder.keys.clone();
    let bytes = decoder.bytes;
    let mut changed = record.clone();
    if let Payload::SelectionTape(s) = &mut changed.event {
        s.record
            .inputs
            .as_mut()
            .unwrap()
            .accounts
            .values_mut()
            .next()
            .unwrap()
            .inflight += 1;
    }
    let pending = encoder.prepare(&changed).unwrap().bytes;
    for rejected in [&compact, &pending] {
        assert!(decoder.decode(rejected).is_err());
        assert!(serde_json::to_vec(&decoder.entries).unwrap() == entries);
        assert!(decoder.keys == hashes);
        assert_eq!(decoder.bytes, bytes);
        assert_eq!(decoder.expanded_bytes, MAX_EXPANDED_EPOCH_BYTES);
    }
    let mut overflow = Decoder {
        expanded_bytes: usize::MAX,
        ..Decoder::default()
    };
    assert!(overflow.decode(&first).is_err());
    assert!(overflow.entries.is_empty() && overflow.keys.is_empty());
    assert_eq!(overflow.bytes, 0);
    assert_eq!(overflow.expanded_bytes, usize::MAX);
}

fn directory() -> tempfile::TempDir {
    super::super::tests::directory()
}

#[test]
fn schema8_actual_enqueue_commits_only_after_budget_and_queue_acceptance() {
    for budget in [false, true] {
        let dir = directory();
        let start = Stamp {
            utc_ns: 1_000_000,
            monotonic_ns: 0,
        };
        let (producer, _writer) = open(&dir.path().join("tape"), Limits::default(), start).unwrap();
        let record = sample();
        {
            let mut state = producer.shared.state.lock().unwrap();
            if budget {
                state.used = HOUR_BYTES;
            } else {
                state.queued_bytes = QUEUE_BYTES;
            }
        }
        assert!(
            producer
                .enqueue(record.at, record.owner, 1, None, record.event.clone())
                .is_none()
        );
        {
            let mut state = producer.shared.state.lock().unwrap();
            assert!(
                state.codec.keys.is_empty() && state.codec.bytes == 0 && state.queue.is_empty()
            );
            state.used = 0;
            state.budget_stopped = false;
            state.queued_bytes = 0;
        }
        assert_eq!(
            producer.enqueue(record.at, record.owner, 2, None, record.event),
            Some(1)
        );
        let state = producer.shared.state.lock().unwrap();
        assert_eq!(state.codec.keys.len(), 4);
        assert_eq!(state.used, state.queue[0].bytes.len() as u64);
        assert!(Decoder::default().decode(&state.queue[0].bytes).is_ok());
    }
}

#[test]
fn schema8_dictionary_record_reference_and_expansion_bounds() {
    let record = sample();
    let mut encoder = Encoder {
        bytes: MAX_DICTIONARY_BYTES,
        ..Encoder::default()
    };
    assert!(encoder.prepare(&record).is_err());
    encoder.bytes = 0;
    encoder.keys = (0..MAX_DEFINITIONS)
        .map(|i| {
            let mut hash = [0; 32];
            hash[..8].copy_from_slice(&(i as u64).to_be_bytes());
            (hash, i as u32)
        })
        .collect();
    assert!(encoder.prepare(&record).is_err());
    let bytes = Encoder::default().prepare(&record).unwrap().bytes;
    let mut decoder = Decoder {
        bytes: MAX_DICTIONARY_BYTES,
        ..Decoder::default()
    };
    assert!(decoder.decode(&bytes).is_err() && decoder.entries.is_empty());
    assert!(
        Decoder::default()
            .decode(&vec![b' '; WIRE_RECORD_BYTES + 1])
            .is_err()
    );
    let expanded = changed_wire(|w| {
        let recent =
            w.record["event"]["facts"]["record"]["inputs"]["selection"]["metrics"]["recent"]
                .as_object_mut()
                .unwrap()
                .values_mut()
                .next()
                .unwrap();
        *recent = Value::Array(vec![recent[0].clone(); 5000]);
    });
    assert!(expanded.len() < RECORD_BYTES);
    assert!(
        Decoder::default().decode(&expanded).is_err(),
        "expanded record must also fit"
    );
    let excessive_refs = changed_wire(|w| {
        let rows = &mut w.record["event"]["facts"]["record"]["inputs"]["selection"]["candidates"];
        *rows = Value::Array(vec![json!({"r":0}); MAX_REFERENCES + 1]);
    });
    assert!(excessive_refs.len() < RECORD_BYTES);
    assert!(Decoder::default().decode(&excessive_refs).is_err());
}

fn large_recent_record(count: usize) -> Record {
    let mut record = sample();
    let Payload::SelectionTape(envelope) = &mut record.event else {
        panic!("selection")
    };
    let inputs = envelope.record.inputs.as_mut().unwrap();
    let now = inputs.now;
    *inputs.selection.metrics.recent.values_mut().next().unwrap() = (0..count)
        .map(|i| {
            (
                now - chrono::Duration::seconds(i as i64),
                format!("codex:{i:032x}"),
            )
        })
        .collect();
    record
}

#[test]
fn schema8_cold_frame_exceeds_logical_cap_and_round_trips_through_queue_writer() {
    let record = large_recent_record(1400);
    let expected = serde_json::to_vec(&record).unwrap();
    assert!(expected.len() > 80_000 && expected.len() < RECORD_BYTES);
    let dir = directory();
    let (producer, mut writer) = open(
        &dir.path().join("tape"),
        Limits::default(),
        Stamp {
            utc_ns: 1_000_000,
            monotonic_ns: 0,
        },
    )
    .unwrap();
    let archive = writer.root.clone();
    assert_eq!(
        producer.enqueue(
            record.at,
            record.owner,
            record.owner_cursor,
            None,
            record.event.clone()
        ),
        Some(1)
    );
    let size = {
        let state = producer.shared.state.lock().unwrap();
        let frame = &state.queue[0].bytes;
        assert!(frame.len() > RECORD_BYTES && frame.len() <= WIRE_RECORD_BYTES);
        assert_eq!(state.used, frame.len() as u64);
        assert_eq!(state.queued_bytes, frame.len());
        frame.len()
    };
    writer.flush(record.at).unwrap();
    let frame =
        zstd::stream::decode_all(fs::read(archive.join(segment_name(0))).unwrap().as_slice())
            .unwrap();
    assert_eq!(frame.len(), size);
    let restored = Decoder::default().decode(&frame).unwrap();
    assert!(serde_json::to_vec(&restored).unwrap() == expected);
}

#[test]
fn schema8_physical_logical_and_queue_limits_are_separate_and_atomic() {
    let over_wire = large_recent_record(3500);
    assert!(serde_json::to_vec(&over_wire).unwrap().len() < RECORD_BYTES);
    let over_logical = large_recent_record(4500);
    assert!(serde_json::to_vec(&over_logical).unwrap().len() >= RECORD_BYTES);
    for record in [over_wire, over_logical] {
        let dir = directory();
        let (producer, _writer) = open(
            &dir.path().join("tape"),
            Limits::default(),
            Stamp {
                utc_ns: 1_000_000,
                monotonic_ns: 0,
            },
        )
        .unwrap();
        assert!(
            producer
                .enqueue(record.at, record.owner, 1, None, record.event)
                .is_none()
        );
        let state = producer.shared.state.lock().unwrap();
        assert!(state.codec.keys.is_empty() && state.codec.bytes == 0 && state.queue.is_empty());
        assert_eq!(state.used, 0);
        assert_eq!(state.queued_bytes, 0);
        assert_eq!(state.manifest.losses.invalid, 1);
    }
    let dir = directory();
    let record = large_recent_record(1400);
    let prepared = Encoder::default().prepare(&record).unwrap();
    let (producer, _writer) = open(
        &dir.path().join("tape"),
        Limits::default(),
        Stamp {
            utc_ns: 1_000_000,
            monotonic_ns: 0,
        },
    )
    .unwrap();
    {
        let mut state = producer.shared.state.lock().unwrap();
        state.queued_bytes = QUEUE_BYTES - prepared.bytes.len() + 1;
    }
    assert!(
        producer
            .enqueue(record.at, record.owner, 1, None, record.event)
            .is_none()
    );
    let state = producer.shared.state.lock().unwrap();
    assert!(state.codec.keys.is_empty() && state.codec.bytes == 0 && state.queue.is_empty());
    assert_eq!(state.used, 0);
    assert_eq!(state.manifest.losses.queue_full, 1);
    drop(state);
    // The physical cap includes its newline: max-1 JSON bytes fit, max do not.
    let mut output = Vec::new();
    {
        let mut sink = BoundedSink::wire(&mut output);
        sink.write_all(&vec![b'x'; WIRE_RECORD_BYTES - 1]).unwrap();
        assert!(sink.write_all(b"x").is_err());
    }
    output.push(b'\n');
    assert_eq!(output.len(), WIRE_RECORD_BYTES);
    assert!(
        Decoder::default()
            .decode(&vec![b' '; WIRE_RECORD_BYTES + 1])
            .is_err()
    );
}

/// Main runs this once after source freeze. No real corpus is checked in.
/// QB_TAPE8_CORPUS_EPOCH: explicit immutable schema-7 epoch directory.
/// QB_TAPE8_CORPUS_EVIDENCE: new JSON file under the project's ignored .sandbox.
#[test]
#[ignore = "requires explicit immutable actual epoch and evidence paths; Main executes"]
fn schema8_actual_retained_corpus_native_roundtrip_and_volume() {
    let input = PathBuf::from(
        std::env::var_os("QB_TAPE8_CORPUS_EPOCH").expect("set QB_TAPE8_CORPUS_EPOCH"),
    )
    .canonicalize()
    .unwrap();
    let evidence = PathBuf::from(
        std::env::var_os("QB_TAPE8_CORPUS_EVIDENCE").expect("set QB_TAPE8_CORPUS_EVIDENCE"),
    );
    assert!(!evidence.exists(), "evidence must be new");
    let parent = evidence.parent().unwrap().canonicalize().unwrap();
    let project = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let main = project.with_file_name("quota-baton");
    assert!(
        parent.starts_with(project.join(".sandbox")) || parent.starts_with(main.join(".sandbox")),
        "evidence must stay under project .sandbox"
    );
    assert!(!parent.starts_with(&input), "input is immutable");
    let before = input_hashes(&input);
    let old_epoch = read_epoch(&input).expect("native schema7 loader must pass first");
    let manifest_bytes = bounded_file(&input.join("manifest.json"), MANIFEST_RESERVE / 2).unwrap();
    let original: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(original.schema, 7);
    let header: Header =
        serde_json::from_slice(&bounded_file(&input.join("header.json"), CONTROL_BYTES).unwrap())
            .unwrap();
    let dir = tempfile::tempdir_in(&parent).unwrap();
    let (producer, mut writer) = open(
        &dir.path().join("reencoded"),
        Limits::default(),
        header.start,
    )
    .unwrap();
    let archive = writer.root.clone();
    let mut decoder = Decoder::default();
    let mut old_bytes = BTreeMap::<String, u64>::new();
    let mut new_bytes = BTreeMap::<String, u64>::new();
    let mut windows = BTreeMap::<u64, u64>::new();
    let mut selection_bytes = Vec::new();
    let mut canonical = Sha256::new();
    let mut restored_hash = Sha256::new();
    let mut records = 0u64;
    let mut executions = 0u64;
    let mut refusals = 0u64;
    let mut last = header.start;
    for segment in &original.segments {
        let compressed =
            bounded_file(&input.join(segment_name(segment.index)), SEGMENT_BYTES).unwrap();
        assert_eq!(digest(&compressed), segment.sha256);
        let reader = zstd::stream::read::Decoder::new(compressed.as_slice()).unwrap();
        let mut reader = io::BufReader::new(reader.take(DECODED_SEGMENT_BYTES as u64 + 1));
        let mut segment_bytes = 0u64;
        loop {
            let mut line = Vec::new();
            let read = reader
                .by_ref()
                .take(RECORD_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)
                .unwrap();
            if read == 0 {
                break;
            }
            assert!(read <= RECORD_BYTES && line.last() == Some(&b'\n'));
            segment_bytes += read as u64;
            assert!(segment_bytes <= DECODED_SEGMENT_BYTES as u64);
            let record: Record = serde_json::from_slice(&line).unwrap();
            let kind = serde_json::to_value(&record.event).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned();
            let expected = serde_json::to_vec(&record).unwrap();
            canonical.update((expected.len() as u64).to_be_bytes());
            canonical.update(&expected);
            // Same actual producer enqueue + staged dictionary commit used live.
            assert_eq!(
                producer.enqueue(
                    record.at,
                    record.owner,
                    record.owner_cursor,
                    record.cause,
                    record.event.clone()
                ),
                Some(record.sequence)
            );
            let encoded = {
                let mut state = producer.shared.state.lock().unwrap();
                // These facts are already projected. Calling selection/permit
                // again would re-key identities; reproduce their accepted-only
                // bookkeeping after the SAME actual enqueue/codec commit.
                accepted_corpus_bookkeeping(&mut state, &record.event);
                state.queue.back().unwrap().bytes.clone()
            };
            let restored = decoder.decode(&encoded).unwrap();
            let actual = serde_json::to_vec(&restored).unwrap();
            assert!(actual == expected, "canonical typed record changed");
            restored_hash.update((actual.len() as u64).to_be_bytes());
            restored_hash.update(&actual);
            *old_bytes.entry(kind.clone()).or_default() += read as u64;
            *new_bytes.entry(kind.clone()).or_default() += encoded.len() as u64;
            *windows
                .entry(record.at.monotonic_ns / HOUR_NS - header.start.monotonic_ns / HOUR_NS)
                .or_default() += encoded.len() as u64;
            if let (Payload::SelectionTape(old), Payload::SelectionTape(new)) =
                (&record.event, &restored.event)
            {
                let a = permit_tape::selection::replay(&old.record);
                let b = permit_tape::selection::replay(&new.record);
                assert!(a == b, "logical selection execution changed");
                if a.is_ok() {
                    executions += 1;
                } else {
                    refusals += 1;
                }
                selection_bytes.push(json!({"before":read,"after":encoded.len()}));
            }
            last = record.at;
            records += 1;
            if producer.shared.state.lock().unwrap().queued_bytes >= GROUP_BYTES {
                writer.flush(last).unwrap();
            }
        }
        assert_eq!(segment_bytes, segment.encoded_bytes);
    }
    let encoded = {
        let state = producer.shared.state.lock().unwrap();
        assert_eq!(state.manifest.permit_records, original.permit_records);
        assert_eq!(state.manifest.selection_records, original.selection_records);
        assert_eq!(
            state.manifest.permit_not_comparable,
            original.permit_not_comparable
        );
        assert_eq!(
            state.manifest.selection_not_comparable,
            original.selection_not_comparable
        );
        assert!(
            state.manifest.permit_not_comparable_reasons == original.permit_not_comparable_reasons
        );
        assert!(
            state.manifest.selection_not_comparable_reasons
                == original.selection_not_comparable_reasons
        );
        state.manifest.encoded_bytes
    };
    writer.close(last).unwrap();
    let new_epoch = read_epoch(&archive).expect("actual schema8 archive reader must pass");
    assert!(
        serde_json::to_value(&old_epoch.permits).unwrap()
            == serde_json::to_value(&new_epoch.permits).unwrap(),
        "archive permit facts changed"
    );
    assert_eq!(old_epoch.selections.len(), new_epoch.selections.len());
    for (old, new) in old_epoch.selections.iter().zip(&new_epoch.selections) {
        assert!(
            serde_json::to_value(old).unwrap() == serde_json::to_value(new).unwrap(),
            "archive selection changed"
        );
        assert!(
            permit_tape::selection::replay(&old.record)
                == permit_tape::selection::replay(&new.record)
        );
    }
    assert_eq!(records, original.persisted);
    assert_eq!(old_bytes.values().sum::<u64>(), original.encoded_bytes);
    assert_eq!(new_bytes.values().sum::<u64>(), encoded);
    let after = input_hashes(&input);
    assert!(before == after, "immutable input changed");
    let canonical = format!("{:x}", canonical.finalize());
    let restored = format!("{:x}", restored_hash.finalize());
    assert_eq!(canonical, restored);
    let span = last.monotonic_ns - header.start.monotonic_ns;
    let state = producer.shared.state.lock().unwrap();
    let result = json!({
        "schema":8,"native_schema7_loader_passed":true,"native_schema8_archive_loader_passed":true,
        "typed_records_verified":records,"canonical_sha256":canonical,"restored_sha256":restored,
        "before_whole_corpus_encoded_bytes":original.encoded_bytes,"after_whole_corpus_encoded_bytes":encoded,
        "before_event_kind_encoded_bytes":old_bytes,"after_event_kind_encoded_bytes":new_bytes,
        "per_selection_encoded_bytes":selection_bytes,"selection_records":old_epoch.selections.len(),
        "comparable_selection_execution_matches":executions,"refused_selection_execution_matches":refusals,
        "dictionary_entries":state.codec.keys.len(),"dictionary_encoded_bytes":state.codec.bytes,
        "retained_window_encoded_bytes":windows,"hourly_budget_unchanged":HOUR_BYTES,
        "header_start_to_last_retained_span_ns":span,"retained_span_normalized_encoded_bytes_per_hour":encoded as f64 * HOUR_NS as f64 / span as f64,
        "original_budget_losses":original.losses.budget,"input_before":before,"input_after":after,
        "retained_only":true,"original_losses":original.losses,
        "reencoded_losses":state.manifest.losses,"reencoded_owner_gap":state.manifest.losses.owner_gap,
        "limitations":"Native reencoding of retained records only. Lost attempts have unknown payloads; no full-hour census or loss-free capture claim. New archive is a temporary test reconstruction, not a replacement capture or power evidence. All definitions/ref/envelope/newline bytes are charged."
    });
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&evidence)
        .unwrap();
    serde_json::to_writer_pretty(&mut file, &result).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();
}

/// Mirror only the publication bookkeeping that public selection()/permit()
/// perform AFTER enqueue_locked succeeds. Never invent or refill lost records.
fn accepted_corpus_bookkeeping(state: &mut TapeState, event: &Payload) {
    let (selection, refusal) = match event {
        Payload::SelectionTape(envelope) => (true, envelope.record.refusal),
        Payload::PermitTape(envelope) => {
            for quota in &envelope.quotas {
                assert!(state.quotas.len() < MAX_QUOTA_SNAPSHOTS);
                assert!(state.quotas.insert(quota.key()));
                state.quota_bytes = state
                    .quota_bytes
                    .checked_add(serde_json::to_vec(quota).unwrap().len())
                    .unwrap();
                assert!(state.quota_bytes <= MAX_QUOTA_DICTIONARY_BYTES);
            }
            let refusal = envelope.record.refusal.or_else(|| {
                envelope.record.inputs.as_ref().and_then(|inputs| {
                    (inputs.from.as_deref().is_some_and(|from| from != inputs.to)
                        && inputs.permit.is_none())
                    .then_some(WireRefusal::MissingPermit)
                })
            });
            (false, refusal)
        }
        _ => return,
    };
    let m = &mut state.manifest;
    if selection {
        m.selection_records += 1;
        if let Some(reason) = refusal {
            increment(&mut m.selection_not_comparable, &mut m.losses.saturated, 1);
            increment(
                m.selection_not_comparable_reasons
                    .entry(reason)
                    .or_default(),
                &mut m.losses.saturated,
                1,
            );
        }
    } else {
        m.permit_records += 1;
        if let Some(reason) = refusal {
            increment(&mut m.permit_not_comparable, &mut m.losses.saturated, 1);
            increment(
                m.permit_not_comparable_reasons.entry(reason).or_default(),
                &mut m.losses.saturated,
                1,
            );
        }
    }
}

fn input_hashes(input: &Path) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(input).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.file_type().unwrap().is_file());
        let name = entry.file_name().into_string().unwrap();
        let limit = if name.ends_with(".zst") {
            SEGMENT_BYTES
        } else {
            MANIFEST_RESERVE / 2
        };
        result.insert(name, digest(&bounded_file(&entry.path(), limit).unwrap()));
    }
    result
}
