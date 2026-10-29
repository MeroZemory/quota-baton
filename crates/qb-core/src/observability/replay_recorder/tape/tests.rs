use super::*;

// Test mutations operate on logical records, then use the current wire codec.
// Otherwise a schema-8 parse error could mask the intended native rejection.
fn decode_current_records(raw: &[u8]) -> Vec<Record> {
    let mut decoder = codec::Decoder::default();
    raw.split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| decoder.decode(line).unwrap())
        .collect()
}

fn encode_current_records(records: &[Record]) -> Vec<u8> {
    let mut encoder = codec::Encoder::default();
    let mut raw = Vec::new();
    for record in records {
        let codec::Prepared { bytes, update } = encoder.prepare(record).unwrap();
        raw.extend_from_slice(&bytes);
        encoder.commit(update);
    }
    raw
}

fn permit_inputs() -> crate::bindings::kernel::DecisionInputs {
    use crate::accounts::{Account, Health, Identity, Source, TokenMeta, UserState};
    use crate::bindings::kernel::{CandidateInput, DecisionInputs, PermitInputs};
    let now: chrono::DateTime<chrono::Utc> = "2026-10-22T03:01:20Z".parse().unwrap();
    let account = Account {
        schema: 1,
        id: "native-source".into(),
        provider: crate::accounts::Provider::Codex,
        label: "private-label-canary".into(),
        email: "private-email-canary@example.invalid".into(),
        plan: Some("pro".into()),
        plan_observed_at: None,
        subscription: None,
        identity: Identity::Codex {
            account_id: "private-identity-canary".into(),
            email: "private-email-canary@example.invalid".into(),
            user_id: None,
        },
        source: Source::Takeover {
            from: PathBuf::from("private-path-canary"),
            at: now,
        },
        user_state: UserState::Active,
        health: Health::Ready,
        auth_failures: 0,
        payment_failures: 0,
        payment_until: None,
        auth_probe_pending: false,
        quota_cooling: None,
        quota_credit_releasable: false,
        drain: false,
        allow_credits: false,
        generation: 7,
        token_meta: TokenMeta::default(),
    };
    DecisionInputs {
        selection: None,
        now,
        model: None,
        key: Some("codex:private-session-canary".into()),
        from: None,
        to: "native-target".into(),
        accounts: BTreeMap::from([(
            "native-source".into(),
            CandidateInput {
                account,
                inflight: 0,
                quota: Some(serde_json::json!({"windows":[
                {"name":"0m", "used_pct":25, "observed_at":now},
                {"name":"43200m", "used_pct":50, "observed_at":now, "reset_at":now+chrono::Duration::days(30)}
            ], "private_unused_field":"private-unused-canary"})),
            },
        )]),
        traffic: BTreeMap::new(),
        pending: BTreeMap::new(),
        groups: BTreeMap::new(),
        bindings: BTreeMap::new(),
        accepted_unbound: BTreeSet::new(),
        sticky: BTreeMap::new(),
        outside_failure: false,
        permit: Some(PermitInputs {
            limits: crate::bindings::MoveGuardLimits::default(),
            now,
            history: Vec::new(),
            global: Vec::new(),
            session_count: 0,
            session_exists: false,
            tripped_until: None,
            revision: 0,
            source_observation_age_ns: Some(0),
            target_observation_age_ns: Some(0),
            sole_target: None,
        }),
    }
}

#[test]
fn permit_envelopes_deduplicate_native_quota_and_round_trip_without_private_metadata() {
    use crate::bindings::kernel::PermitDecision;
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    let inputs = permit_inputs();
    assert_eq!(
        producer.permit(at(2), 1, &inputs, &PermitDecision::NotRequired),
        Some(2)
    );
    assert_eq!(
        producer.permit(at(3), 2, &inputs, &PermitDecision::NotRequired),
        Some(3)
    );
    {
        let state = producer.shared.state.lock().unwrap();
        assert_eq!(state.quotas.len(), 1);
        let raw: Vec<u8> = state
            .queue
            .iter()
            .flat_map(|q| q.bytes.iter().copied())
            .collect();
        let records = decode_current_records(&raw);
        let envelopes: Vec<_> = records
            .iter()
            .filter_map(|r| match &r.event {
                Payload::PermitTape(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(envelopes[0].quotas.len(), 1);
        assert!(envelopes[1].quotas.is_empty());
        let bytes: Vec<_> = state
            .queue
            .iter()
            .flat_map(|q| q.bytes.iter().copied())
            .collect();
        let encoded = String::from_utf8(bytes).unwrap();
        for canary in [
            "private-label-canary",
            "private-email-canary",
            "private-identity-canary",
            "private-path-canary",
            "private-unused-canary",
            "private-session-canary",
            "native-source",
        ] {
            assert!(
                !encoded.contains(canary),
                "private metadata reached native queue: {canary}"
            );
        }
    }
    writer.close(at(4)).unwrap();
    let epoch = read_epoch(&path).unwrap();
    assert!(epoch.permit_complete);
    assert_eq!(epoch.permits.len(), 2);
    assert_eq!(epoch.losses, 0);
    for record in &epoch.permits {
        assert_eq!(
            permit_tape::replay(record).unwrap(),
            PermitDecision::NotRequired
        );
        let inputs = record.inputs.as_ref().unwrap().to_inputs().unwrap();
        let quota = inputs
            .accounts
            .values()
            .next()
            .unwrap()
            .quota
            .as_ref()
            .unwrap();
        assert_eq!(quota["windows"][0]["name"], "0m");
        assert_eq!(quota["windows"][1]["name"], "43200m");
    }
}

#[test]
fn failed_envelope_publication_does_not_commit_the_quota_dictionary() {
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    let inputs = permit_inputs();
    producer.shared.state.lock().unwrap().budget_stopped = true;
    assert!(
        producer
            .permit(
                at(2),
                1,
                &inputs,
                &crate::bindings::kernel::PermitDecision::NotRequired
            )
            .is_none()
    );
    {
        let mut state = producer.shared.state.lock().unwrap();
        assert!(state.quotas.is_empty());
        assert_eq!(state.quota_bytes, 0);
        state.budget_stopped = false;
    }
    assert_eq!(
        producer.permit(
            at(3),
            2,
            &inputs,
            &crate::bindings::kernel::PermitDecision::NotRequired
        ),
        Some(2)
    );
    writer.close(at(4)).unwrap();
    let epoch = read_epoch(&path).unwrap();
    assert!(!epoch.permit_complete);
    assert_eq!(epoch.permits.len(), 1);
    assert_eq!(manifest(&path).losses.budget, 1);
}

#[test]
fn permit_commit_markers_require_an_earlier_allowed_native_proposal_and_are_single_use() {
    for invalid in [false, true] {
        let (_directory, producer, writer) = fixture();
        let path = writer.root.clone();
        witness(&producer);
        let mut inputs = permit_inputs();
        inputs.from = Some("native-source".into());
        let actual = crate::bindings::kernel::decide_move(&inputs);
        assert!(matches!(
            &actual,
            crate::bindings::kernel::PermitDecision::Allowed(_)
        ));
        assert_eq!(producer.permit(at(2), 1, &inputs, &actual), Some(2));
        assert_eq!(producer.permit_commit(at(3), 2, 2), Some(3));
        if invalid {
            assert_eq!(producer.permit_commit(at(4), 3, 2), Some(4));
        }
        writer.close(at(5)).unwrap();
        if invalid {
            assert!(read_epoch(&path).is_err());
        } else {
            let epoch = read_epoch(&path).unwrap();
            assert!(epoch.permit_complete);
            assert_eq!(epoch.permit_commits, 1);
        }
    }
}

#[test]
fn refused_input_is_retained_and_counted_without_manufacturing_a_move() {
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    let mut inputs = permit_inputs();
    inputs.model = Some("private-model-canary".into());
    assert_eq!(
        producer.permit(
            at(2),
            1,
            &inputs,
            &crate::bindings::kernel::PermitDecision::NotRequired
        ),
        Some(2)
    );
    writer.close(at(3)).unwrap();
    let epoch = read_epoch(&path).unwrap();
    assert!(epoch.permit_complete);
    assert_eq!(epoch.permits.len(), 1);
    assert!(epoch.permits[0].inputs.is_none());
    assert_eq!(
        epoch.permits[0].refusal,
        Some(permit_tape::WireRefusal::Model)
    );
    assert_eq!(manifest(&path).permit_not_comparable, 1);
    assert_eq!(
        epoch.permits[0].actual,
        crate::bindings::kernel::PermitDecision::NotRequired
    );
}
use crate::{
    bindings::decision_tape::{Entry, Inputs, LaunchInputs, ReadState, RebindMarker},
    observability::{
        decision_comparison::{Outcome, OutcomeKind},
        replay_projection as projection,
    },
};

pub(super) fn directory() -> tempfile::TempDir {
    let executable = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let root = executable
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/n6r-tape-tests");
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
fn at(n: u64) -> Stamp {
    Stamp {
        utc_ns: 1_000_000 + n as i64,
        monotonic_ns: n,
    }
}
fn fixture() -> (tempfile::TempDir, Producer, Writer) {
    let directory = directory();
    let (producer, writer) =
        open(&directory.path().join("tape"), Limits::default(), at(0)).unwrap();
    (directory, producer, writer)
}
fn decision() -> DecisionRecord {
    DecisionRecord {
        schema: 1,
        ordinal: 999,
        capture_id: None,
        inputs: Inputs {
            entry: Entry::Chooser,
            at: chrono::Utc::now(),
            provider: crate::accounts::Provider::Codex,
            key: Some("private-session-canary".into()),
            parent: None,
            model: Some("private-model-canary".into()),
            pinned: Some("private-account-canary".into()),
            request_bytes: 17,
            hold_allowed: false,
            hold_timed_out: false,
            coverage: None,
            root_hint_present: false,
            launch_metadata_valid: None,
            launch: None,
        },
        state: Some(ReadState {
            candidates: Vec::new(),
            bound: Some("private-account-canary".into()),
            hold_until: None,
            outside_unknown: false,
            capacity_available: true,
            unsupported: Vec::new(),
        }),
        answers: Vec::new(),
        rebind: RebindMarker::default(),
        outcome: Outcome {
            kind: OutcomeKind::None,
            account: None,
            until: None,
            refusal: None,
            moves: 0,
        },
        capture_aborted: false,
        read_state_complete: false,
        selection_ordinals: Vec::new(),
        selection_refusals: 0,
    }
}
fn witness(p: &Producer) {
    assert_eq!(
        p.metadata(
            at(1),
            Owner::Runtime,
            1,
            None,
            Event::InitialWitness {
                component: Owner::Bindings,
                revision: 1,
                cursor: 0,
                rows: 0,
                digest: Fingerprint(Sha256::digest(b"synthetic writer fixture image").into()),
            }
        ),
        Some(1)
    );
}
fn manifest(path: &Path) -> Manifest {
    serde_json::from_slice(
        &bounded_file(&path.join("manifest.json"), MANIFEST_RESERVE / 2).unwrap(),
    )
    .unwrap()
}
fn facts(p: &Producer) -> projection::AccountFacts {
    projection::AccountFacts {
        account: p
            .identity(IdentityDomain::Account, b"private-account-canary")
            .unwrap(),
        provider: Provider::Codex,
        order_rank: None,
        generation: 1,
        state: projection::State::Active,
        health: projection::HealthKind::Ready,
        reason: None,
        scope: projection::Scope::Account,
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
    }
}

#[test]
fn typed_roundtrip_assigns_archive_ordinal_and_protects_identity_fields() {
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    let original = decision();
    assert_eq!(producer.decision(at(2), 1, &original), Some(2));
    assert_eq!(
        original.ordinal, 999,
        "caller observation must remain untouched"
    );
    writer.close(at(3)).unwrap();
    let epoch = read_epoch(&path).unwrap();
    assert!(
        !epoch.complete,
        "current native capture is explicitly partial"
    );
    assert_eq!(epoch.losses, 0);
    assert_eq!(epoch.producer_build.as_ref().unwrap().len(), 64);
    assert_eq!(epoch.decisions.len(), 1);
    let record = &epoch.decisions[0];
    assert_eq!(record.ordinal, 2);
    assert_eq!(record.inputs.pinned, record.state.as_ref().unwrap().bound);
    assert_eq!(record.inputs.pinned.as_ref().unwrap().len(), 32);
    let m = manifest(&path);
    let compressed = bounded_file(&path.join(segment_name(0)), SEGMENT_BYTES).unwrap();
    let decoded = zstd::stream::decode_all(compressed.as_slice()).unwrap();
    let text = String::from_utf8(decoded).unwrap();
    for canary in [
        "private-account-canary",
        "private-session-canary",
        "private-model-canary",
    ] {
        assert!(!text.contains(canary));
    }
    assert!(
        !text.contains("\"capture\""),
        "capture belongs in header, not every row"
    );
    assert_eq!(m.persisted, 2);
    assert_eq!(m.segments[0].sha256, digest(&compressed));
    assert_eq!(m.compressed_bytes, compressed.len() as u64);
    assert_eq!(epoch.tape_sha256, read_epoch(&path).unwrap().tape_sha256);
}

#[test]
fn account_state_dedup_tracks_changes_but_not_rejected_observations() {
    let (_directory, producer, mut writer) = fixture();
    let path = writer.root.clone();
    let mut account = facts(&producer);
    assert_eq!(
        producer.metadata(
            at(1),
            Owner::Runtime,
            1,
            None,
            Event::AccountState { facts: account }
        ),
        Some(1)
    );
    assert_eq!(
        producer.metadata(
            at(2),
            Owner::Runtime,
            2,
            None,
            Event::AccountState { facts: account }
        ),
        None
    );
    account.generation += 1;
    producer.shared.state.lock().unwrap().queued_bytes = QUEUE_BYTES;
    assert_eq!(
        producer.metadata(
            at(3),
            Owner::Runtime,
            3,
            None,
            Event::AccountState { facts: account }
        ),
        None
    );
    writer.flush(at(4)).unwrap();
    assert_eq!(
        producer.metadata(
            at(5),
            Owner::Runtime,
            4,
            None,
            Event::AccountState { facts: account }
        ),
        Some(2)
    );
    writer.close(at(6)).unwrap();
    let m = manifest(&path);
    assert_eq!(m.deduplicated, 1);
    assert_eq!(m.losses.queue_full, 1);
    assert_eq!(m.persisted, 2);
    assert!(!read_epoch(&path).unwrap().complete);
}

#[test]
fn hourly_stop_go_counts_every_refusal_and_resumes_without_resetting_loss() {
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    producer.shared.state.lock().unwrap().used = HOUR_BYTES - 1000;
    let mut large = decision();
    large.inputs.coverage = Some(vec!["x".repeat(64); 64]);
    assert_eq!(producer.decision(at(1), 1, &large), None);
    let small = Event::InitialWitness {
        component: Owner::Bindings,
        revision: 1,
        cursor: 0,
        rows: 0,
        digest: Fingerprint([1; 32]),
    };
    let small_bytes = serde_json::to_vec(&Record {
        sequence: 1,
        at: at(2),
        owner: Owner::Runtime,
        owner_cursor: 1,
        cause: None,
        event: Payload::Metadata(Box::new(small)),
    })
    .unwrap();
    assert!(
        small_bytes.len() + 1 < 1000,
        "control must fit unused remainder"
    );
    assert_eq!(
        producer.metadata(at(2), Owner::Runtime, 1, None, small),
        None
    );
    assert!(producer.shared.state.lock().unwrap().enabled);
    assert!(producer.shared.state.lock().unwrap().budget_stopped);
    assert_eq!(producer.decision(at(HOUR_NS), 2, &decision()), Some(1));
    writer.close(at(HOUR_NS + 1)).unwrap();
    let m = manifest(&path);
    assert_eq!(m.losses.budget, 2);
    assert_eq!(m.persisted, 1);
    assert!(m.budget_window_used <= HOUR_BYTES);
    let epoch = read_epoch(&path).unwrap();
    assert!(!epoch.complete);
    assert_eq!(epoch.losses, 2);
}

#[test]
fn closed_launch_refusals_and_aborted_capture_are_retained_without_raw_errors() {
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    let reasons = [
        "invalid_metadata",
        "anchor_conflict",
        "unknown_launch",
        "no_eligible_account",
        "entitlements_unknown",
        "model_unsupported",
        "consent_required",
        "busy",
        "capacity",
        "policy_blocked",
        "move_guard",
        "binding_changed",
        "retired",
        "capture_aborted",
    ];
    for (n, reason) in reasons.iter().enumerate() {
        let mut record = decision();
        record.inputs.entry = Entry::Launch;
        record.inputs.launch = Some(LaunchInputs {
            resume: Some("private-session-canary".into()),
            model_is_explicit: true,
            account_preference: Some("private-account-canary".into()),
            allow_move_for_model: false,
            operation_present: false,
            admission_present: false,
            entitlements: vec![(
                "private-account-canary".into(),
                vec!["private-model-canary".into()],
            )],
        });
        record.outcome.kind = OutcomeKind::Refusal;
        record.outcome.refusal = Some((*reason).into());
        record.capture_aborted = *reason == "capture_aborted";
        assert_eq!(
            producer.decision(at(n as u64 + 2), n as u64 + 1, &record),
            Some(n as u64 + 2)
        );
    }
    let mut record = decision();
    record.outcome.kind = OutcomeKind::Refusal;
    record.outcome.refusal = Some("secret arbitrary error canary".into());
    assert_eq!(producer.decision(at(20), 15, &record), None);
    writer.close(at(21)).unwrap();
    let epoch = read_epoch(&path).unwrap();
    assert_eq!(epoch.decisions.len(), reasons.len());
    let first = &epoch.decisions[0];
    let launch = first.inputs.launch.as_ref().unwrap();
    assert_eq!(first.inputs.pinned, launch.account_preference);
    assert_eq!(first.inputs.key, launch.resume);
    assert_eq!(
        first.inputs.pinned.as_ref().unwrap(),
        &launch.entitlements[0].0
    );
    assert_eq!(
        first.inputs.model.as_ref().unwrap(),
        &launch.entitlements[0].1[0]
    );
    assert!(!epoch.complete);
    assert_eq!(manifest(&path).losses.capture_aborted, 1);
    assert_eq!(manifest(&path).losses.invalid, 1);
    assert_eq!(
        epoch.decisions.last().unwrap().outcome.refusal.as_deref(),
        Some("capture_aborted")
    );
}

#[test]
fn missing_witness_and_unsealed_epoch_never_claim_completeness() {
    let (_directory, producer, mut writer) = fixture();
    let path = writer.root.clone();
    assert_eq!(producer.decision(at(1), 1, &decision()), Some(1));
    writer.flush(at(2)).unwrap();
    assert!(!read_epoch(&path).unwrap().complete);
    writer.close(at(3)).unwrap();
    let epoch = read_epoch(&path).unwrap();
    assert!(!epoch.complete);
    assert!(epoch.initial_witness.is_empty());
}

#[test]
fn complete_frames_can_be_appended_then_tampered_bytes_are_rejected() {
    let (_directory, producer, mut writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    writer.flush(at(2)).unwrap();
    assert_eq!(producer.decision(at(3), 1, &decision()), Some(2));
    writer.flush(at(4)).unwrap();
    writer.close(at(5)).unwrap();
    assert_eq!(read_epoch(&path).unwrap().decisions.len(), 1);
    let file = path.join(segment_name(0));
    let mut bytes = fs::read(&file).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(file, bytes).unwrap();
    assert!(read_epoch(&path).is_err());
}

#[test]
fn overlong_typed_record_is_rejected_before_queue_allocation() {
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    let mut record = decision();
    record.inputs.key = Some("x".repeat(RECORD_BYTES + 1));
    assert_eq!(producer.decision(at(1), 1, &record), None);
    assert!(producer.shared.state.lock().unwrap().queue.is_empty());
    writer.close(at(2)).unwrap();
    assert_eq!(manifest(&path).losses.invalid, 1);
}

#[test]
fn legacy_schema3_archive_is_untouched_and_new_root_can_start() {
    let directory = directory();
    let legacy = directory.path().join("routing-replay");
    let (old_producer, old_writer) = Recorder::enable_opt_in(
        &legacy,
        BuildIdentity::default(),
        Provenance::Synthetic,
        at(0),
    )
    .unwrap();
    assert!(matches!(
        old_producer.record(
            at(1),
            Owner::Runtime,
            1,
            None,
            Event::InitialWitness {
                component: Owner::Bindings,
                revision: 1,
                cursor: 0,
                rows: 0,
                digest: Fingerprint([1; 32])
            }
        ),
        RecordResult::Queued(_)
    ));
    old_writer.close(at(2)).unwrap();
    let epoch = legacy.join("e00");
    let before: BTreeMap<_, _> = fs::read_dir(&epoch)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect();
    let (_producer, writer) = open(
        &directory.path().join("decision-tape-v4"),
        Limits::default(),
        at(0),
    )
    .unwrap();
    writer.close(at(1)).unwrap();
    let after: BTreeMap<_, _> = fs::read_dir(&epoch)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect();
    assert_eq!(before, after);
    assert!(
        read_epoch(&epoch).is_err(),
        "schema3 must not be reinterpreted as schema4"
    );
}

#[test]
fn failed_sync_never_advances_durable_receipts_or_seals_epoch() {
    let (_directory, producer, mut writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    assert_eq!(producer.decision(at(2), 1, &decision()), Some(2));
    writer.fail_sync = true;
    assert!(writer.flush(at(3)).is_err());
    let m = manifest(&path);
    assert!(!m.sealed);
    assert_eq!(m.persisted, 0);
    assert_eq!(m.compressed_bytes, 0);
    assert_eq!(m.losses.unpersisted, 2);
    assert_eq!(m.losses.write_error, 1);
    assert!(!producer.shared.state.lock().unwrap().enabled);
    assert!(regular_file(&path.join(segment_name(0))).unwrap() > 0);
    assert!(read_epoch(&path).is_err());
    drop(writer);
    let (_successor, next) = open(path.parent().unwrap(), Limits::default(), at(4)).unwrap();
    next.close(at(5)).unwrap();
}

#[test]
fn reader_rejects_bad_ordinal_even_with_matching_compressed_hash() {
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    assert_eq!(producer.decision(at(2), 1, &decision()), Some(2));
    writer.close(at(3)).unwrap();
    let mut m = manifest(&path);
    let segment = path.join(segment_name(0));
    let bytes = fs::read(&segment).unwrap();
    let raw = zstd::stream::decode_all(bytes.as_slice()).unwrap();
    let mut records = decode_current_records(&raw);
    if let Payload::DecisionTape(d) = &mut records[1].event {
        d.ordinal = 99;
    }
    let raw = encode_current_records(&records);
    let bytes = zstd::stream::encode_all(raw.as_slice(), 3).unwrap();
    fs::write(segment, &bytes).unwrap();
    m.segments[0].sha256 = digest(&bytes);
    m.segments[0].compressed_bytes = bytes.len() as u64;
    m.segments[0].encoded_bytes = raw.len() as u64;
    m.compressed_bytes = bytes.len() as u64;
    m.encoded_bytes = raw.len() as u64;
    fs::write(path.join("manifest.json"), serde_json::to_vec(&m).unwrap()).unwrap();
    assert!(read_epoch(&path).is_err());
}

#[test]
fn schema6_producer_failure_reasons_count_once_even_after_disable() {
    use ProducerFailureReason::*;
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    producer.disable();
    let reasons = [
        Unspecified,
        RuntimeIdExhausted,
        MetadataCursorExhausted,
        DecisionCursorExhausted,
        PermitCursorExhausted,
        CommitCursorExhausted,
        CommitWithoutProposal,
        StartupActiveState,
        StartupImageSerialization,
        StartupAccountsSerialization,
        StartupPublication,
    ];
    for reason in reasons {
        producer.failed(reason);
    }
    writer.close(at(1)).unwrap();
    let m = manifest(&path);
    assert_eq!(m.schema, TAPE_SCHEMA);
    assert_eq!(m.losses.producer_failure, reasons.len() as u64);
    assert_eq!(m.producer_failure_reasons.len(), reasons.len());
    for reason in reasons {
        assert_eq!(m.producer_failure_reasons[&reason], 1);
    }
    let epoch = read_epoch(&path).unwrap();
    assert_eq!(epoch.losses, reasons.len() as u64);
    assert!(!epoch.complete);
    assert!(!epoch.permit_complete);
}

#[test]
fn schema6_native_refusals_count_the_record_reason_or_missing_permit() {
    use crate::bindings::kernel::PermitDecision;
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    let mut inputs = permit_inputs();
    inputs.model = Some("private-model-canary".into());
    assert!(
        producer
            .permit(at(2), 1, &inputs, &PermitDecision::NotRequired)
            .is_some()
    );
    inputs.model = None;
    inputs.from = Some("native-source".into());
    inputs.permit = None;
    assert!(
        producer
            .permit(at(3), 2, &inputs, &PermitDecision::NotRequired)
            .is_some()
    );
    writer.close(at(4)).unwrap();
    let m = manifest(&path);
    assert_eq!(m.permit_not_comparable, 2);
    assert_eq!(
        m.permit_not_comparable_reasons,
        BTreeMap::from([(WireRefusal::Model, 1), (WireRefusal::MissingPermit, 1),])
    );
    let epoch = read_epoch(&path).unwrap();
    assert_eq!(epoch.permits[0].refusal, Some(WireRefusal::Model));
    assert!(epoch.permits[0].inputs.is_none());
    assert!(epoch.permits[1].refusal.is_none());
    assert!(epoch.permits[1].inputs.as_ref().unwrap().permit.is_none());
    assert!(
        epoch
            .permits
            .iter()
            .all(|p| p.actual == PermitDecision::NotRequired)
    );
}

#[test]
fn schema6_rejects_diagnostic_tampering_omissions_unknown_keys_and_overflow() {
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    let mut inputs = permit_inputs();
    inputs.model = Some("private-model-canary".into());
    assert!(
        producer
            .permit(
                at(2),
                1,
                &inputs,
                &crate::bindings::kernel::PermitDecision::NotRequired
            )
            .is_some()
    );
    producer.failed(ProducerFailureReason::StartupPublication);
    writer.close(at(3)).unwrap();
    let original: serde_json::Value =
        serde_json::from_slice(&fs::read(path.join("manifest.json")).unwrap()).unwrap();
    assert!(read_epoch(&path).is_ok());
    for case in 0..9 {
        let mut tampered = original.clone();
        match case {
            0 => {
                tampered["permit_not_comparable_reasons"] = serde_json::json!({"model": 2});
            }
            // Same total, wrong record reason must also fail.
            1 => {
                tampered["permit_not_comparable_reasons"] = serde_json::json!({"plan": 1});
            }
            2 => {
                tampered["producer_failure_reasons"] =
                    serde_json::json!({"startup_publication": 2});
            }
            3 => {
                tampered["producer_failure_reasons"] =
                    serde_json::json!({"private-error-canary": 1});
            }
            4 => {
                tampered["permit_not_comparable_reasons"] =
                    serde_json::json!({"private-quota-canary": 1});
            }
            5 => {
                tampered
                    .as_object_mut()
                    .unwrap()
                    .remove("permit_not_comparable_reasons");
            }
            6 => {
                tampered
                    .as_object_mut()
                    .unwrap()
                    .remove("producer_failure_reasons");
            }
            7 => {
                tampered["producer_failure_reasons"] =
                    serde_json::json!({"unspecified": u64::MAX, "runtime_id_exhausted": 2});
            }
            8 => {
                tampered["permit_not_comparable_reasons"] =
                    serde_json::json!({"model": u64::MAX, "plan": 2});
            }
            _ => unreachable!(),
        }
        fs::write(
            path.join("manifest.json"),
            serde_json::to_vec(&tampered).unwrap(),
        )
        .unwrap();
        assert!(read_epoch(&path).is_err(), "accepted tamper case {case}");
    }
    fs::write(
        path.join("manifest.json"),
        serde_json::to_vec(&original).unwrap(),
    )
    .unwrap();
    assert!(read_epoch(&path).is_ok());
}

#[test]
fn schema6_writer_refuses_legacy5_root_without_rewriting_it() {
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    let mut inputs = permit_inputs();
    assert!(
        producer
            .permit(
                at(2),
                1,
                &inputs,
                &crate::bindings::kernel::PermitDecision::NotRequired
            )
            .is_some()
    );
    inputs.model = Some("private-model-canary".into());
    assert!(
        producer
            .permit(
                at(3),
                2,
                &inputs,
                &crate::bindings::kernel::PermitDecision::NotRequired
            )
            .is_some()
    );
    writer.close(at(4)).unwrap();
    // Construct an actual legacy payload, not schema-8 envelopes wearing a
    // schema-5 header. Only this fixture is converted; frozen old bytes below
    // remain unchanged and retain their independent expected epoch hashes.
    let mut legacy = manifest(&path);
    assert_eq!(legacy.segments.len(), 1);
    let segment_path = path.join(segment_name(0));
    let current = zstd::stream::decode_all(fs::read(&segment_path).unwrap().as_slice()).unwrap();
    let records = decode_current_records(&current);
    let mut raw = Vec::new();
    for record in records {
        serde_json::to_writer(&mut raw, &record).unwrap();
        raw.push(b'\n');
    }
    let compressed = zstd::stream::encode_all(raw.as_slice(), 3).unwrap();
    fs::write(segment_path, &compressed).unwrap();
    legacy.segments[0].encoded_bytes = raw.len() as u64;
    legacy.segments[0].compressed_bytes = compressed.len() as u64;
    legacy.segments[0].sha256 = digest(&compressed);
    legacy.encoded_bytes = raw.len() as u64;
    legacy.compressed_bytes = compressed.len() as u64;
    legacy.budget_window_used = raw.len() as u64;
    fs::write(
        path.join("manifest.json"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    for name in ["epoch.json", "header.json", "manifest.json"] {
        let p = path.join(name);
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        value["schema"] = 5.into();
        if name == "manifest.json" {
            value
                .as_object_mut()
                .unwrap()
                .remove("permit_not_comparable_reasons");
            value
                .as_object_mut()
                .unwrap()
                .remove("producer_failure_reasons");
        }
        fs::write(p, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    let before: BTreeMap<_, _> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name(),
                if e.file_type().unwrap().is_file() {
                    fs::read(e.path()).unwrap()
                } else {
                    Vec::new()
                },
            )
        })
        .collect();
    let loaded = crate::observability::decision_reader::load(&path).unwrap();
    assert_eq!(loaded.permits.len(), 2);
    assert_eq!(loaded.permits[1].refusal, Some(WireRefusal::Model));
    let epoch_before = loaded.tape_sha256;
    assert!(open(path.parent().unwrap(), Limits::default(), at(5)).is_err());
    assert_eq!(read_epoch(&path).unwrap().tape_sha256, epoch_before);
    for (name, bytes) in before {
        let p = path.parent().unwrap().join(name);
        if p.is_file() {
            assert_eq!(fs::read(p).unwrap(), bytes);
        }
    }
}

#[test]
fn unchanged_legacy5_bytes_keep_the_frozen_hash_without_new_diagnostics() {
    // Frozen old controls, deliberately not serialized through the schema 6 DTO.
    let directory = directory();
    let path = directory.path().join("legacy5");
    fs::create_dir(&path).unwrap();
    let epoch = br#"{"schema":5,"start_utc_ns":1000000}"#;
    let header = br#"{"schema":5,"start":{"utc_ns":1000000,"monotonic_ns":0},"capture":"frozen-legacy5","identity_domain_version":1,"build":{},"provenance":"Synthetic"}"#;
    let manifest = br#"{"schema":5,"sealed":true,"capture":"frozen-legacy5","attempted":0,"accepted":0,"persisted":0,"deduplicated":0,"permit_records":0,"permit_not_comparable":0,"encoded_bytes":0,"compressed_bytes":0,"hourly_encoded_byte_limit":20000000,"budget_window_ns":0,"budget_window_used":0,"budget_window_stopped":false,"losses":{"capture_aborted":0,"budget":0,"queue_full":0,"invalid":0,"clock_regressed":0,"owner_gap":0,"producer_failure":0,"disk_cap":0,"write_error":0,"unpersisted":0,"saturated":false},"segments":[]}"#;
    for (name, bytes) in [
        ("epoch.json", epoch.as_slice()),
        ("header.json", header.as_slice()),
        ("manifest.json", manifest.as_slice()),
    ] {
        fs::write(path.join(name), bytes).unwrap();
    }
    let loaded = crate::observability::decision_reader::load(&path).unwrap();
    assert_eq!(
        loaded.tape_sha256,
        "063ccf9dd022eba9259fae549b1cd414e74b798ace08ccb19b8781110cb917e3"
    );
    assert_eq!(loaded.losses, 0);
    assert!(loaded.permits.is_empty());
    assert!(!loaded.complete);
    assert!(!loaded.permit_complete);
    assert_eq!(fs::read(path.join("manifest.json")).unwrap(), manifest);

    let v6 = directory.path().join("schema6");
    fs::create_dir(&v6).unwrap();
    for (name, bytes) in [
        ("epoch.json", epoch.as_slice()),
        ("header.json", header.as_slice()),
        ("manifest.json", manifest.as_slice()),
    ] {
        let text = std::str::from_utf8(bytes).unwrap()
            .replace("\"schema\":5", "\"schema\":6")
            .replace("\"segments\":[]", "\"permit_not_comparable_reasons\":{},\"producer_failure_reasons\":{},\"segments\":[]");
        fs::write(v6.join(name), text).unwrap();
    }
    let loaded6 = crate::observability::decision_reader::load(&v6).unwrap();
    assert_eq!(
        loaded6.tape_sha256,
        "d7fec5cebdcc37123c05a115e5d5848bbde4e6f0605860e6cb6b80ff76b8410a"
    );
    assert_ne!(loaded6.tape_sha256, loaded.tape_sha256);

    let v7 = directory.path().join("schema7");
    fs::create_dir(&v7).unwrap();
    for (name, bytes) in [
        ("epoch.json", epoch.as_slice()),
        ("header.json", header.as_slice()),
        ("manifest.json", manifest.as_slice()),
    ] {
        let text = std::str::from_utf8(bytes).unwrap()
            .replace("\"schema\":5", "\"schema\":7")
            .replace("\"segments\":[]", "\"permit_not_comparable_reasons\":{},\"producer_failure_reasons\":{},\"selection_records\":0,\"selection_not_comparable\":0,\"selection_not_comparable_reasons\":{},\"segments\":[]");
        fs::write(v7.join(name), text).unwrap();
    }
    let controls_before: Vec<_> = ["epoch.json", "header.json", "manifest.json"]
        .iter()
        .map(|name| fs::read(v7.join(name)).unwrap())
        .collect();
    let loaded7 = read_epoch(&v7).unwrap();
    assert_eq!(
        loaded7.tape_sha256,
        "912c50c026d34cfbdac91a4eddb15330d5fcff7500b9fbf03d81ad90861ab42f"
    );
    for (name, before) in ["epoch.json", "header.json", "manifest.json"]
        .iter()
        .zip(controls_before)
    {
        assert_eq!(fs::read(v7.join(name)).unwrap(), before);
    }
}

fn rewrite_selection_fixture(path: &Path, change: impl FnOnce(&mut [Record])) {
    let mut m = manifest(path);
    assert_eq!(m.segments.len(), 1);
    let segment = path.join(segment_name(0));
    let raw = zstd::stream::decode_all(fs::read(&segment).unwrap().as_slice()).unwrap();
    let mut rows = decode_current_records(&raw);
    change(&mut rows);
    let raw = encode_current_records(&rows);
    let compressed = zstd::stream::encode_all(raw.as_slice(), 3).unwrap();
    fs::write(segment, &compressed).unwrap();
    m.segments[0].sha256 = digest(&compressed);
    m.segments[0].compressed_bytes = compressed.len() as u64;
    m.segments[0].encoded_bytes = raw.len() as u64;
    m.compressed_bytes = compressed.len() as u64;
    m.encoded_bytes = raw.len() as u64;
    m.budget_window_used = raw.len() as u64;
    fs::write(path.join("manifest.json"), serde_json::to_vec(&m).unwrap()).unwrap();
}

fn linked_refused_selections() -> (tempfile::TempDir, PathBuf) {
    use crate::bindings::kernel::selection::{SelectionChoice, SelectionDecision, SelectionLabel};
    let (directory, producer, writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    let actual = SelectionDecision {
        choice: SelectionChoice::None,
        label: SelectionLabel::New,
        rebalance: None,
        sole_target: None,
        budget_source: None,
    };
    // This native input intentionally lacks a selection read set. The producer
    // emits a typed Shape refusal rather than fabricating a comparable input.
    let inputs = permit_inputs();
    assert_eq!(
        producer.selection(at(2), 1, Entry::Chooser, 101, 1, &inputs, &actual),
        Some(2)
    );
    assert_eq!(
        producer.selection(at(3), 2, Entry::Chooser, 102, 1, &inputs, &actual),
        Some(3)
    );
    for (cursor, id, ordinal) in [(3, 101, 2), (4, 102, 3)] {
        let mut parent = decision();
        parent.capture_id = Some(id);
        parent.selection_ordinals = vec![ordinal];
        assert!(producer.decision(at(cursor + 1), cursor, &parent).is_some());
    }
    writer.close(at(6)).unwrap();
    let epoch = read_epoch(&path).unwrap();
    assert!(epoch.selection_complete);
    assert_eq!(epoch.selections.len(), 2);
    assert!(
        epoch
            .selections
            .iter()
            .all(|s| s.record.refusal == Some(WireRefusal::Shape))
    );
    (directory, path)
}

#[test]
fn schema7_refused_selection_still_requires_inner_schema_and_projected_actual() {
    for malformed_schema in [true, false] {
        let (_directory, path) = linked_refused_selections();
        rewrite_selection_fixture(&path, |rows| {
            let Payload::SelectionTape(envelope) = &mut rows[1].event else {
                panic!("selection missing")
            };
            if malformed_schema {
                envelope.record.schema = 99;
            } else {
                envelope.record.actual.budget_source = Some("raw-private-account-canary".into());
            }
        });
        assert!(
            read_epoch(&path).is_err(),
            "matching compressed hashes must not waive codec validation"
        );
    }
}

#[test]
fn schema7_selection_links_reject_cross_capture_exchange() {
    let (_directory, path) = linked_refused_selections();
    rewrite_selection_fixture(&path, |rows| {
        let Payload::DecisionTape(first) = &mut rows[3].event else {
            panic!("first parent missing")
        };
        first.selection_ordinals = vec![3];
        let Payload::DecisionTape(second) = &mut rows[4].event else {
            panic!("second parent missing")
        };
        second.selection_ordinals = vec![2];
    });
    assert!(
        read_epoch(&path).is_err(),
        "entry and invocation equality alone do not prove parent identity"
    );
}

#[test]
fn schema6_native_quota_refusals_have_finite_manifest_counters() {
    use crate::bindings::kernel::PermitDecision;
    use WireRefusal::*;
    use serde_json::json;
    let (_directory, producer, writer) = fixture();
    let path = writer.root.clone();
    witness(&producer);
    let cases = [
        (QuotaRoot, json!(false)),
        (QuotaWindows, json!({"windows": {}})),
        (QuotaWindow, json!({"windows": [false]})),
        (
            QuotaWindowName,
            json!({"windows": [{"name": "private-window-canary"}]}),
        ),
        (
            QuotaWindowUsed,
            json!({"windows": [{"used_pct": "private-used-canary"}]}),
        ),
        (
            QuotaWindowStatus,
            json!({"windows": [{"status": "private-status-canary"}]}),
        ),
        (
            QuotaWindowReset,
            json!({"windows": [{"reset_at": "private-reset-canary"}]}),
        ),
        (
            QuotaWindowObserved,
            json!({"windows": [{"observed_at": "private-observed-canary"}]}),
        ),
        (QuotaCredits, json!({"credits": false})),
        (
            QuotaCreditFlag,
            json!({"credits": {"has_credits": "private-flag-canary"}}),
        ),
        (
            QuotaCreditFlag,
            json!({"credits": {"unlimited": "private-flag-canary"}}),
        ),
        (
            QuotaCreditBalance,
            json!({"credits": {"balance": "private-balance-canary"}}),
        ),
        (
            QuotaPaidServing,
            json!({"paid_serving_observed_at": "private-paid-canary"}),
        ),
    ];
    let mut expected = BTreeMap::new();
    let mut inputs = permit_inputs();
    for (index, (reason, quota)) in cases.into_iter().enumerate() {
        inputs.accounts.get_mut("native-source").unwrap().quota = Some(quota);
        let cursor = index as u64 + 1;
        assert!(
            producer
                .permit(
                    at(cursor + 1),
                    cursor,
                    &inputs,
                    &PermitDecision::NotRequired
                )
                .is_some()
        );
        *expected.entry(reason).or_insert(0u64) += 1;
    }
    writer.close(at(20)).unwrap();
    let m = manifest(&path);
    assert_eq!(m.permit_not_comparable, 13);
    assert_eq!(m.permit_not_comparable_reasons, expected);
    let epoch = read_epoch(&path).unwrap();
    assert_eq!(epoch.permits.len(), 13);
    for record in epoch.permits {
        assert!(record.inputs.is_none());
        assert!(record.refusal.is_some());
        assert_eq!(record.actual, PermitDecision::NotRequired);
    }
    let wire = String::from_utf8(fs::read(path.join("manifest.json")).unwrap()).unwrap();
    assert!(!wire.contains("private-"));
}
