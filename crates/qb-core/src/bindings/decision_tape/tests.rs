use super::*;
use crate::bindings::tests::candidate;

fn fixture_dir() -> tempfile::TempDir {
    if let Some(parent) = std::env::var_os("QB_TEST_USAGE_ROOT") {
        // The gate supplies an existing private writable parent. TempDir creates
        // a private child; no fixture writes into a read-only source checkout.
        tempfile::Builder::new()
            .prefix("n6r-bindings-")
            .tempdir_in(parent)
            .unwrap()
    } else {
        crate::bindings::tests::fixture_dir()
    }
}

fn recorder(table: &Arc<BindingTable>) -> Arc<Mutex<Vec<DecisionRecord>>> {
    let records = Arc::new(Mutex::new(Vec::new()));
    let sink = records.clone();
    let owner = Arc::downgrade(table);
    table.set_decision_recorder(Arc::new(move |record| {
        let table = owner.upgrade().unwrap();
        assert!(
            matches!(
                table.mutations.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ),
            "native mutation gate must cover finish and aborted publication"
        );
        let mut records = sink.lock().unwrap();
        records.push(record.clone());
        Some(records.len() as u64)
    }));
    records
}

// The same native pin/admission owners used by the move-guard regressions.
#[allow(clippy::too_many_arguments)]
fn pinned_choice(
    table: &Arc<BindingTable>,
    key: &str,
    candidates: &[Candidate],
    sources: &[Candidate],
    observations: MoveObservations,
    pin: &str,
    now: DateTime<Utc>,
) -> Decision {
    let pins = crate::debug_pin::DebugPins::default();
    pins.set(Provider::Claude, Some(key.into()), pin.into());
    let proof = pins.selection(Provider::Claude, Some(key)).unwrap();
    table.choose_with_hold_at_admitted_proof(
        Provider::Claude,
        Some(key),
        None,
        None,
        candidates,
        sources,
        observations,
        Some(pin),
        100,
        false,
        false,
        now,
        None,
        |_| Some(DispatchAdmission::ordinary().with_pin(Some(proof))),
    )
}

#[test]
fn native_all_outcomes_rebind_completeness_and_synthetic_archive() {
    use crate::observability::{
        decision_reader,
        replay_recorder::{Event, Fingerprint, Limits, Owner, Stamp, tape},
    };
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let image = table.decision_tape_initial_image().unwrap();
    let archive_root = if let Some(path) = std::env::var_os("QB_N6R_SYNTHETIC_ARCHIVE") {
        let path = PathBuf::from(path);
        assert!(
            path.is_absolute(),
            "archive export needs an absolute new path"
        );
        let reviewed = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .join("quota-baton/.sandbox/reports/N6r-evidence")
            .canonicalize()
            .unwrap();
        let parent = path.parent().unwrap().canonicalize().unwrap();
        assert!(
            parent.starts_with(reviewed),
            "archive parent must be under MAIN N6r evidence"
        );
        let path = parent.join(path.file_name().unwrap());
        assert_eq!(
            fs::symlink_metadata(&path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound,
            "archive export must not replace an existing path"
        );
        path
    } else {
        root.path().join("synthetic-tape")
    };
    let start = std::time::Instant::now();
    let utc = Utc::now().timestamp_nanos_opt().unwrap();
    let clock = move || {
        let elapsed = u64::try_from(start.elapsed().as_nanos()).unwrap();
        Stamp {
            utc_ns: utc.checked_add(i64::try_from(elapsed).unwrap()).unwrap(),
            monotonic_ns: elapsed,
        }
    };
    let (producer, writer) = tape::open(&archive_root, Limits::default(), clock()).unwrap();
    assert!(
        producer
            .metadata(
                clock(),
                Owner::Runtime,
                1,
                None,
                Event::InitialWitness {
                    component: Owner::Bindings,
                    revision: image.revision,
                    cursor: 0,
                    rows: image.bindings_rows,
                    digest: Fingerprint(Sha256::digest(serde_json::to_vec(&image).unwrap()).into()),
                }
            )
            .is_some()
    );
    let records = Arc::new(Mutex::new(Vec::<DecisionRecord>::new()));
    let sink = records.clone();
    let output = producer.clone();
    let owner = Arc::downgrade(&table);
    table.set_decision_recorder(Arc::new(move |record| {
        let table = owner.upgrade().unwrap();
        assert!(
            matches!(
                table.mutations.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ),
            "native freeze gate must cover archive enqueue"
        );
        let mut records = sink.lock().unwrap();
        records.push(record.clone());
        output.decision(clock(), records.len() as u64, record)
    }));
    let now = Utc::now();
    let cs = [
        candidate("cl-111111", 10.0, 10, 2),
        candidate("cl-222222", 20.0, 20, 3),
        candidate("cl-333333", 30.0, 30, 4),
    ];
    let key = "claude:synthetic-tape-move";
    let first = pinned_choice(
        &table,
        key,
        &cs,
        &cs,
        MoveObservations::same(MoveObservation::before_collection()),
        &cs[0].account.id,
        now,
    );
    let Decision::Selected {
        account,
        lease,
        moves,
    } = first
    else {
        panic!("initial native placement failed")
    };
    assert_eq!(account, cs[0].account.id);
    assert_eq!(replacements(&moves), 0);
    lease.dispatched();
    let old_observed = MoveObservation::before_collection();
    let mut old = cs.clone();
    old[1].account.apply_health_signal(
        HealthSignal::Cooling {
            reason: HealthReason::Payment,
            until: now + chrono::Duration::hours(6),
            scope: "account".into(),
        },
        now,
    );
    let moved = pinned_choice(
        &table,
        key,
        &cs,
        &cs,
        MoveObservations::same(MoveObservation::before_collection()),
        &cs[1].account.id,
        now,
    );
    let Decision::Selected {
        account,
        lease,
        moves,
    } = moved
    else {
        panic!("actual native replacement failed")
    };
    assert_eq!(account, cs[1].account.id);
    assert_eq!(replacements(&moves), 1);
    lease.dispatched();
    assert!(matches!(
        pinned_choice(
            &table,
            key,
            &old[2..],
            &old,
            MoveObservations::same(old_observed),
            &old[2].account.id,
            now
        ),
        Decision::MoveHeld { .. }
    ));
    assert_eq!(
        table.current(Some(key)).as_deref(),
        Some(cs[1].account.id.as_str())
    );

    let hold_key = "claude:synthetic-tape-hold";
    let placed = pinned_choice(
        &table,
        hold_key,
        &cs,
        &cs,
        MoveObservations::same(MoveObservation::before_collection()),
        &cs[0].account.id,
        now,
    );
    let Decision::Selected { lease, .. } = placed else {
        panic!("native hold setup failed")
    };
    lease.dispatched();
    let until = now + chrono::Duration::seconds(10);
    let mut cooling = cs[0].clone();
    cooling.account.health = Health::Cooling {
        reason: HealthReason::Quota,
        until,
        scope: "account".into(),
    };
    cooling.account.quota_cooling = Some(crate::accounts::QuotaCooling {
        window: "5h".into(),
        reset_at: until,
        started_at: now,
    });
    assert!(
        matches!(table.choose_with_hold_at(Provider::Claude,Some(hold_key),None,None,&[cooling],None,100,true,false,now),Decision::Hold { until: actual } if actual==until)
    );
    assert!(matches!(
        table.choose(Provider::Claude, None, None, None, &[], None, 0),
        Decision::None
    ));
    let original_weight = {
        let mut d = table.data.lock().unwrap();
        let weight = d.snapshot_weight;
        d.snapshot_weight = SNAPSHOT_MAX_BYTES as usize;
        weight
    };
    let blocked = table.choose(
        Provider::Claude,
        Some("claude:synthetic-tape-capacity"),
        None,
        None,
        &cs,
        None,
        100,
    );
    table.data.lock().unwrap().snapshot_weight = original_weight;
    assert!(matches!(blocked, Decision::Blocked));
    let request = LaunchRequest {
        anchor_key: "invalid",
        resume_key: None,
        model: None,
        model_is_explicit: false,
        account_pref: None,
        allow_move_for_model: false,
    };
    assert!(matches!(
        table.place_launch(request, &[], &LaunchEntitlements::new(), now),
        Err(LaunchError::InvalidMetadata)
    ));
    let request = LaunchRequest {
        anchor_key: "launch:synthetic-tape-launch",
        ..request
    };
    assert!(matches!(
        table.place_launch(request, &[], &LaunchEntitlements::new(), now),
        Err(LaunchError::NoEligibleAccount)
    ));
    let mut codex = candidate("cx-444444", 10.0, 168, 5);
    codex.account.provider = Provider::Codex;
    codex.account.identity = crate::accounts::Identity::Codex {
        account_id: "synthetic-tape-account".into(),
        email: "synthetic@example.invalid".into(),
        user_id: None,
    };
    let launched = table
        .place_launch(request, &[codex], &LaunchEntitlements::new(), now)
        .unwrap();
    assert_eq!(launched.account, "cx-444444");

    let records = records.lock().unwrap();
    let expected = [
        OutcomeKind::Dispatch,
        OutcomeKind::Dispatch,
        OutcomeKind::MoveHeld,
        OutcomeKind::Dispatch,
        OutcomeKind::Hold,
        OutcomeKind::None,
        OutcomeKind::Blocked,
        OutcomeKind::Refusal,
        OutcomeKind::Refusal,
        OutcomeKind::Dispatch,
    ];
    assert_eq!(
        records.len(),
        expected.len(),
        "one captured frame per actual entry"
    );
    for (record, kind) in records.iter().zip(expected) {
        assert_eq!(record.outcome.kind, kind);
        assert!(!record.capture_aborted);
        assert!(!record.read_state_complete);
        assert_eq!(replay_record(record), Err(ReplayRefusal::Unsupported));
        if kind == OutcomeKind::Dispatch {
            assert_eq!(record.rebind.entered, 1);
            assert_eq!(record.rebind.completed, 1);
        } else {
            assert_eq!(record.rebind.entered, 0);
            assert_eq!(record.rebind.completed, 0);
        }
    }
    assert_eq!(records.iter().map(|r| r.outcome.moves).sum::<u64>(), 1);
    assert_eq!(
        records.iter().map(|r| r.rebind.replacements).sum::<u64>(),
        1
    );
    assert!(!records[6].state.as_ref().unwrap().capacity_available);
    assert_eq!(
        records[7].outcome.refusal.as_deref(),
        Some("invalid_metadata")
    );
    assert_eq!(
        records[8].outcome.refusal.as_deref(),
        Some("no_eligible_account")
    );
    assert_eq!(table.decision_tape_losses(), 0);
    writer.close(clock()).unwrap();
    let epochs: Vec<_> = fs::read_dir(&archive_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(epochs.len(), 1);
    let epoch = decision_reader::load(&epochs[0]).unwrap();
    assert_eq!(epoch.decisions.len(), records.len());
    assert_eq!(epoch.losses, 0);
    assert!(
        !epoch.complete,
        "partial native read inputs must never seal as replay-complete"
    );
    assert!(
        epoch
            .decisions
            .windows(2)
            .all(|pair| pair[0].ordinal < pair[1].ordinal)
    );
    assert_eq!(
        epoch.decisions.iter().map(|r| r.outcome.moves).sum::<u64>(),
        1
    );
    // The producer artifact also attempts the reference replay. This is an
    // actual evaluator invocation, not an execution made from observed outputs.
    use std::io::Read;
    let mut binary = fs::File::open(std::env::current_exe().unwrap()).unwrap();
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = binary.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let build = format!("{:x}", hash.finalize());
    assert_eq!(epoch.producer_build.as_deref(), Some(build.as_str()));
    let reference = decision_reader::execute(&epoch, build, None).unwrap();
    assert_eq!(reference.run.unsupported, epoch.decisions.len() as u64);
    assert!(reference.run.frames.is_empty());
    if std::env::var_os("QB_N6R_SYNTHETIC_ARCHIVE").is_some() {
        use std::io::Write;
        let path = archive_root
            .parent()
            .unwrap()
            .join("synthetic-reference-run.json");
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .unwrap();
        file.write_all(&serde_json::to_vec_pretty(&reference).unwrap())
            .unwrap();
        file.sync_all().unwrap();
    }
    eprintln!("N6R_SYNTHETIC_EPOCH={}", epochs[0].display());
}

#[test]
fn startup_image_fingerprints_attached_outside_state_instead_of_rejecting_it() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let missing = table.decision_tape_initial_image().unwrap();
    let failed = Arc::new(AtomicBool::new(false));
    {
        let mut d = table.data.lock().unwrap();
        d.outside_failure = Some(failed.clone());
        d.outside_used.insert("sample-outside-account".into());
    }
    let attached = table.decision_tape_initial_image().unwrap();
    assert_ne!(missing.outside_sha256, attached.outside_sha256);
    assert!(!attached.replay_complete);
    assert!(attached.activity_empty);
    failed.store(true, Ordering::Release);
    let failure = table.decision_tape_initial_image().unwrap();
    assert_ne!(attached.outside_sha256, failure.outside_sha256);
    table.data.lock().unwrap().outside_unknown = true;
    assert_ne!(
        failure.outside_sha256,
        table.decision_tape_initial_image().unwrap().outside_sha256
    );
    let encoded = serde_json::to_string(&attached).unwrap();
    assert!(!encoded.contains("sample-outside-account"));
}

#[test]
fn native_entry_records_none_and_real_rebind_without_echo_replay() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let image = table.decision_tape_initial_image().unwrap();
    assert!(!image.replay_complete);
    assert_eq!(image.bindings_rows, 0);
    let records = recorder(&table);
    assert!(matches!(
        table.choose(Provider::Claude, None, None, None, &[], None, 0),
        Decision::None
    ));
    let c = candidate("sample-tape-account", 10.0, 168, 5);
    let result = table.choose(
        Provider::Claude,
        Some("claude:sample-tape-session"),
        None,
        None,
        &[c],
        None,
        100,
    );
    let Decision::Selected { lease, .. } = result else {
        panic!("native fixture was not dispatched")
    };
    lease.dispatched();
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].outcome.kind, OutcomeKind::None);
    assert_eq!(records[0].rebind.entered, 0);
    assert_eq!(records[1].outcome.kind, OutcomeKind::Dispatch);
    assert_eq!(records[1].rebind.entered, 1);
    assert_eq!(records[1].rebind.completed, 1);
    assert_eq!(records[1].rebind.replacements, 0);
    assert_eq!(records[1].outcome.moves, 0);
    assert_eq!(records[1].answers.last().unwrap().check, Check::Authorize);
    assert!(!records[1].read_state_complete);
    let encoded = serde_json::to_string(&*records).unwrap();
    for private in [
        "sample-tape-account",
        "sample-tape-session",
        "synthetic@example.invalid",
    ] {
        assert!(!encoded.contains(private));
    }
    let mut changed = records[1].clone();
    changed.outcome.kind = OutcomeKind::Blocked;
    assert_eq!(replay_record(&changed), Err(ReplayRefusal::Unsupported));
    assert_eq!(replay_record(&records[1]), Err(ReplayRefusal::Unsupported));
    assert_eq!(table.decision_tape_losses(), 0);
}

#[test]
fn native_rejected_admission_and_launch_error_are_not_dropped() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let records = recorder(&table);
    let c = candidate("sample-tape-account", 10.0, 168, 5);
    let result = table.choose_with_hold_at_admitted(
        Provider::Claude,
        None,
        None,
        None,
        &[c],
        None,
        100,
        false,
        false,
        Utc::now(),
        |_| false,
    );
    assert!(matches!(result, Decision::None));
    let launch = table.place_launch(
        LaunchRequest {
            anchor_key: "invalid",
            resume_key: None,
            model: None,
            model_is_explicit: false,
            account_pref: None,
            allow_move_for_model: false,
        },
        &[],
        &LaunchEntitlements::new(),
        Utc::now(),
    );
    assert!(matches!(launch, Err(LaunchError::InvalidMetadata)));
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].answers.len(), 1);
    assert_eq!(records[0].answers[0].check, Check::Admission);
    assert!(!records[0].answers[0].value);
    assert_eq!(records[0].rebind.entered, 0);
    assert_eq!(records[1].outcome.kind, OutcomeKind::Refusal);
    assert_eq!(
        records[1].outcome.refusal.as_deref(),
        Some("invalid_metadata")
    );
    assert_eq!(records[1].rebind.entered, 0);
}

#[test]
fn native_entry_unwind_is_retained_and_sticky_loss_is_visible() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let records = recorder(&table);
    let c = candidate("sample-tape-account", 10.0, 168, 5);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        table.choose_with_hold_at_admitted(
            Provider::Claude,
            None,
            None,
            None,
            &[c],
            None,
            100,
            false,
            false,
            Utc::now(),
            |_| panic!("synthetic admission unwind"),
        )
    }));
    assert!(result.is_err());
    let captured = records.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(captured[0].capture_aborted);
    assert_eq!(
        captured[0].outcome.refusal.as_deref(),
        Some("capture_aborted")
    );
    assert!(!captured[0].read_state_complete);
    assert_eq!(table.decision_tape_losses(), 1);
    drop(captured);
    assert!(matches!(
        table.choose(Provider::Claude, None, None, None, &[], None, 0),
        Decision::None
    ));
    let captured = records.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert!(!captured[1].capture_aborted);
    assert_eq!(captured[1].outcome.kind, OutcomeKind::None);
    assert_eq!(captured[1].rebind.entered, 0);
    assert_eq!(table.decision_tape_losses(), 1);
}

#[test]
fn recorder_rejection_or_panic_does_not_change_native_result_and_counts_loss() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    table.set_decision_recorder(Arc::new(|_| None));
    assert!(matches!(
        table.choose(Provider::Claude, None, None, None, &[], None, 0),
        Decision::None
    ));
    assert_eq!(table.decision_tape_losses(), 1);
    table.set_decision_recorder(Arc::new(|_| panic!("synthetic recorder failure")));
    assert!(matches!(
        table.choose(Provider::Claude, None, None, None, &[], None, 0),
        Decision::None
    ));
    assert_eq!(table.decision_tape_losses(), 2);
}
