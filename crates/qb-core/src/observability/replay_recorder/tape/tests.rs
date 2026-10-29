use super::*;

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
        let records: Vec<Record> = state
            .queue
            .iter()
            .map(|q| serde_json::from_slice(&q.bytes).unwrap())
            .collect();
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

fn directory() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/n6r-tape-tests");
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
    let mut records: Vec<Record> = raw
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .map(|s| serde_json::from_slice(s).unwrap())
        .collect();
    if let Payload::DecisionTape(d) = &mut records[1].event {
        d.ordinal = 99;
    }
    let mut raw = Vec::new();
    for r in records {
        serde_json::to_writer(&mut raw, &r).unwrap();
        raw.push(b'\n');
    }
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
