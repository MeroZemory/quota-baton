use super::*;
use std::sync::atomic::AtomicU64;

struct Fixture {
    registry: LaunchTokenRegistry,
    seconds: Arc<AtomicU64>,
}

fn utc() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap()
}

fn metadata() -> LaunchMetadata {
    LaunchMetadata {
        binding_anchor_key: "original-anchor:1".into(),
        advertised_models: BTreeSet::from(["original-model".into()]),
    }
}

impl Fixture {
    fn new() -> Self {
        let origin = Instant::now();
        let seconds = Arc::new(AtomicU64::new(0));
        let ticks = seconds.clone();
        Self {
            registry: LaunchTokenRegistry::with_clock(Arc::new(move || {
                origin + Duration::from_secs(ticks.load(Ordering::SeqCst))
            })),
            seconds,
        }
    }

    fn advance(&self, seconds: u64) {
        self.seconds.fetch_add(seconds, Ordering::SeqCst);
    }

    fn issue(&self, seed: u8) -> LaunchToken {
        self.registry
            .issue_with(metadata(), || Ok(Zeroizing::new([seed; 32])))
            .unwrap()
    }

    fn snapshot(&self) -> HandoverSnapshot {
        self.registry.export_handover_at(utc()).unwrap()
    }

    fn restore(
        &self,
        snapshot: HandoverSnapshot,
        downtime: u64,
    ) -> Result<LaunchTokenRegistry, HandoverError> {
        LaunchTokenRegistry::restore_handover_at(
            snapshot,
            &[],
            utc() + chrono::Duration::seconds(downtime as i64),
            self.registry.0.clock.clone(),
        )
    }
}

#[test]
fn old_token_roundtrips_to_exact_anchor_without_plaintext_or_runtime_ownership() {
    let f = Fixture::new();
    let token = f.issue(7);
    let bytes = f.snapshot().to_json().unwrap();
    assert!(
        !bytes
            .windows(token.expose_secret().len())
            .any(|w| w == token.expose_secret().as_bytes())
    );
    let snapshot = HandoverSnapshot::from_json(&bytes).unwrap();
    assert_eq!(
        snapshot.binding_anchor_keys().collect::<Vec<_>>(),
        ["original-anchor:1"]
    );
    let restored = f.restore(snapshot, 9).unwrap();
    let digest = token_digest(token.expose_secret());
    {
        let entries = restored.0.lock();
        let entry = &entries[&digest];
        assert_eq!(entry.pins, 0);
        assert!(entry.deferred_owner.upgrade().is_none());
        assert_eq!(
            entry.binding_activity.load(Ordering::Acquire),
            utc().timestamp_millis()
        );
    }
    assert_eq!(
        restored
            .get_or_pin(token.expose_secret())
            .unwrap()
            .metadata(),
        metadata()
    );
    assert!(!format!("{restored:?} {token:?}").contains(token.expose_secret()));
    // A default caller has no transferred authority, even with the real old URL.
    assert_eq!(
        LaunchTokenRegistry::default()
            .get_or_pin(token.expose_secret())
            .unwrap_err(),
        LaunchTokenError::Missing
    );
}

#[test]
fn idle_age_and_utc_downtime_do_not_reset_lifetime() {
    let f = Fixture::new();
    let token = f.issue(1);
    f.advance(IDLE_TIMEOUT.as_secs() - 20);
    let restored = f.restore(f.snapshot(), 11).unwrap();
    assert_eq!(restored.0.lock().len(), 1);
    f.advance(9);
    assert_eq!(
        restored.get_or_pin(token.expose_secret()).unwrap_err(),
        LaunchTokenError::Expired
    );

    let f = Fixture::new();
    f.issue(1);
    f.advance(IDLE_TIMEOUT.as_secs() - 20);
    assert!(f.restore(f.snapshot(), 20).unwrap().0.lock().is_empty());
    assert!(f.restore(f.snapshot(), 21).unwrap().0.lock().is_empty());
    f.advance(20);
    assert!(f.snapshot().rows.is_empty());
}

#[test]
fn active_old_pin_or_owner_becomes_only_a_bounded_idle_grace() {
    for owner_live in [false, true] {
        let f = Fixture::new();
        let token = f.issue(2);
        let pin = f.registry.get_or_pin(token.expose_secret()).unwrap();
        let owner = owner_live.then(|| pin.open_deferred_owner().unwrap());
        let pin = if owner_live {
            drop(pin);
            None
        } else {
            Some(pin)
        };
        f.advance(IDLE_TIMEOUT.as_secs() * 3);
        let snapshot = f.snapshot();
        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(
            snapshot.rows[0].remaining_idle_nanos,
            IDLE_TIMEOUT.as_nanos() as u64
        );
        let restored = f.restore(snapshot, 5).unwrap();
        {
            let entries = restored.0.lock();
            let entry = &entries[&token_digest(token.expose_secret())];
            assert_eq!(entry.pins, 0);
            assert!(entry.deferred_owner.upgrade().is_none());
        }
        f.advance(IDLE_TIMEOUT.as_secs() - 5);
        assert_eq!(
            restored.get_or_pin(token.expose_secret()).unwrap_err(),
            LaunchTokenError::Expired
        );
        drop((owner, pin));
    }
}

#[test]
fn future_negative_and_impossible_time_fail_closed() {
    let f = Fixture::new();
    f.issue(1);
    let mut snapshot = f.snapshot();
    snapshot.exported_at_utc += chrono::Duration::nanoseconds(1);
    assert_eq!(
        f.restore(snapshot, 0).unwrap_err(),
        HandoverError::Timestamp
    );
    let mut snapshot = f.snapshot();
    snapshot.exported_at_utc = DateTime::from_timestamp(-1, 0).unwrap();
    assert_eq!(
        f.restore(snapshot, 0).unwrap_err(),
        HandoverError::Timestamp
    );
    let mut snapshot = f.snapshot();
    snapshot.rows[0].remaining_idle_nanos = IDLE_TIMEOUT.as_nanos() as u64 + 1;
    assert_eq!(f.restore(snapshot, 0).unwrap_err(), HandoverError::Invalid);
    let mut snapshot = f.snapshot();
    snapshot.rows[0].remaining_idle_nanos = 0;
    assert_eq!(f.restore(snapshot, 0).unwrap_err(), HandoverError::Invalid);
}

fn extra_row(digest: [u8; 32]) -> Row {
    Row {
        token_sha256: digest,
        metadata: metadata(),
        box_scope: None,
        remaining_idle_nanos: IDLE_TIMEOUT.as_nanos() as u64,
    }
}

#[test]
fn duplicate_collision_metadata_and_capacity_fail_atomically() {
    let f = Fixture::new();
    let token = f.issue(1);
    let mut duplicate = f.snapshot();
    duplicate
        .rows
        .push(extra_row(duplicate.rows[0].token_sha256));
    assert_eq!(
        f.restore(duplicate, 0).unwrap_err(),
        HandoverError::Duplicate
    );
    for invalid in ["", "bad anchor", "https://secret.invalid"] {
        let mut snapshot = f.snapshot();
        snapshot.rows[0].metadata.binding_anchor_key = invalid.into();
        assert_eq!(f.restore(snapshot, 0).unwrap_err(), HandoverError::Invalid);
    }
    for invalid in ["", "bad model", "line\nbreak"] {
        let mut snapshot = f.snapshot();
        snapshot.rows[0].metadata.advertised_models = BTreeSet::from([invalid.into()]);
        assert_eq!(f.restore(snapshot, 0).unwrap_err(), HandoverError::Invalid);
    }
    let mut snapshot = f.snapshot();
    snapshot.schema += 1;
    assert_eq!(f.restore(snapshot, 0).unwrap_err(), HandoverError::Schema);
    let mut snapshot = f.snapshot();
    for n in 0..CAPACITY {
        snapshot
            .rows
            .push(extra_row(Sha256::digest(n.to_le_bytes()).into()));
    }
    assert_eq!(f.restore(snapshot, 0).unwrap_err(), HandoverError::Bounds);
    // None of these constructors can alter or partially merge into a live table.
    assert_eq!(f.registry.0.lock().len(), 1);
    assert_eq!(
        f.registry
            .get_or_pin(token.expose_secret())
            .unwrap()
            .metadata(),
        metadata()
    );
}

#[test]
fn exact_capacity_is_accepted_and_json_decode_rejects_over_capacity() {
    let f = Fixture::new();
    let mut snapshot = f.snapshot();
    for n in 0..CAPACITY {
        snapshot
            .rows
            .push(extra_row(Sha256::digest(n.to_le_bytes()).into()));
    }
    let bytes = snapshot.to_json().unwrap();
    assert_eq!(
        f.restore(HandoverSnapshot::from_json(&bytes).unwrap(), 0)
            .unwrap()
            .0
            .lock()
            .len(),
        CAPACITY
    );
    snapshot.rows.push(extra_row([255; 32]));
    // Deliberately serialize an invalid test DTO into a wiping transport buffer.
    let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_HANDOVER_BYTES));
    serde_json::to_writer(&mut *bytes, &snapshot).unwrap();
    assert!(HandoverSnapshot::from_json(&bytes).is_err());
}

#[test]
fn strict_schema_digest_and_total_json_bounds() {
    for input in [
        br#"{"schema":1,"exported_at_utc":"2027-01-15T08:00:00Z","rows":[],"unexpected":0}"#.as_slice(),
        br#"{"schema":1,"exported_at_utc":"2027-01-15T08:00:00Z","rows":[{"token_sha256":[1],"metadata":{"binding_anchor_key":"a","advertised_models":[]},"box_scope":null,"remaining_idle_nanos":1}]}"#.as_slice(),
        br#"{"schema":1,"exported_at_utc":"impossible","rows":[]}"#.as_slice(),
    ] {
        assert!(HandoverSnapshot::from_json(input).is_err());
    }
    let f = Fixture::new();
    f.issue(1);
    let bytes = f.snapshot().to_json().unwrap();
    // Operate only on wiping byte buffers, never serde_json::Value copies.
    for (needle, replacement) in [
        (b"\"box_scope\":null,".as_slice(), b"".as_slice()),
        (
            b"\"metadata\":{".as_slice(),
            b"\"metadata\":{\"extra\":0,".as_slice(),
        ),
        (
            b"\"token_sha256\":".as_slice(),
            b"\"unexpected\":0,\"token_sha256\":".as_slice(),
        ),
    ] {
        let at = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap();
        let mut invalid = Zeroizing::new(Vec::with_capacity(bytes.len() + replacement.len()));
        invalid.extend_from_slice(&bytes[..at]);
        invalid.extend_from_slice(replacement);
        invalid.extend_from_slice(&bytes[at + needle.len()..]);
        assert!(HandoverSnapshot::from_json(&invalid).is_err());
    }
    let oversized = Zeroizing::new(vec![b' '; MAX_HANDOVER_BYTES + 1]);
    assert!(matches!(
        HandoverSnapshot::from_json(&oversized),
        Err(HandoverError::Bounds)
    ));
    let f = Fixture::new();
    let mut snapshot = f.snapshot();
    for n in 0..300usize {
        let mut row = extra_row(Sha256::digest(n.to_le_bytes()).into());
        row.metadata.advertised_models = (0..256)
            .map(|m| format!("{m:04}{}", "x".repeat(252)))
            .collect();
        snapshot.rows.push(row);
    }
    assert!(matches!(snapshot.to_json(), Err(HandoverError::Bounds)));
    // The actual export path must refuse too, before accumulating the full map.
    {
        let mut entries = f.registry.0.lock();
        for row in &snapshot.rows {
            entries.insert(
                row.token_sha256,
                Entry {
                    metadata: row.metadata.clone(),
                    box_scope: None,
                    last_activity: (f.registry.0.clock)(),
                    pins: 0,
                    deferred_owner: Weak::new(),
                    binding_activity: Arc::new(AtomicI64::new(utc().timestamp_millis())),
                },
            );
        }
    }
    assert!(matches!(
        f.registry.export_handover_at(utc()),
        Err(HandoverError::Bounds)
    ));
    assert_eq!(f.restore(snapshot, 0).unwrap_err(), HandoverError::Bounds);
}

#[test]
fn scoped_grant_clones_exact_registry_scope_without_tcp_or_epoch_weakening() {
    use crate::boxes::{BoxName, BoxRegistry};
    let base = std::env::var_os("QB_TEST_USAGE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/tests/launch-handover")
        });
    std::fs::create_dir_all(&base).unwrap();
    let base = std::fs::canonicalize(base).unwrap();
    let root = tempfile::tempdir_in(base).unwrap();
    let home = root.path().join("home");
    crate::platform::create_private_directory_new(&home).unwrap();
    let boxes = BoxRegistry::open(&home).unwrap();
    let a = BoxName::parse("a").unwrap();
    let (old_a, _) = boxes.add(a.clone()).unwrap().into_parts();
    let (b, _) = boxes
        .add(BoxName::parse("b").unwrap())
        .unwrap()
        .into_parts();
    boxes.revoke(&a).unwrap();
    let (new_a, _) = boxes.add(a).unwrap().into_parts();
    let f = Fixture::new();
    let token = f
        .registry
        .issue_with_scope(metadata(), Some(old_a.clone()), || {
            Ok(Zeroizing::new([9; 32]))
        })
        .unwrap();
    let scopes = [old_a.clone(), b.clone(), new_a.clone()];
    let restored = LaunchTokenRegistry::restore_handover_at(
        f.snapshot(),
        &scopes,
        utc(),
        f.registry.0.clock.clone(),
    )
    .unwrap();
    for expected in [None, Some(&b), Some(&new_a)] {
        assert_eq!(
            restored
                .get_or_pin_scoped(token.expose_secret(), expected)
                .unwrap_err(),
            LaunchTokenError::ScopeMismatch
        );
    }
    assert_eq!(
        restored
            .get_or_pin_scoped(token.expose_secret(), Some(&old_a))
            .unwrap()
            .metadata(),
        metadata()
    );
    for scopes in [
        &[][..],
        std::slice::from_ref(&b),
        std::slice::from_ref(&new_a),
    ] {
        let restored = LaunchTokenRegistry::restore_handover_at(
            f.snapshot(),
            scopes,
            utc(),
            f.registry.0.clock.clone(),
        )
        .unwrap();
        assert!(restored.0.lock().is_empty());
        assert_eq!(
            restored.get_or_pin(token.expose_secret()).unwrap_err(),
            LaunchTokenError::Missing
        );
    }
}

#[test]
fn owned_snapshot_fields_are_wiped_on_cleanup() {
    let f = Fixture::new();
    f.issue(3);
    let mut snapshot = f.snapshot();
    // Exercise the actual cleanup used by Drop, retaining it for assertions.
    let row = &mut snapshot.rows[0];
    row.box_scope = Some(Scope {
        name: "secret-box".into(),
        epoch: 23,
    });
    row.zeroize();
    assert_eq!(row.token_sha256, [0; 32]);
    assert!(row.metadata.binding_anchor_key.is_empty());
    assert!(row.metadata.advertised_models.is_empty());
    assert!(row.box_scope.as_ref().unwrap().name.is_empty());
    assert_eq!(row.box_scope.as_ref().unwrap().epoch, 0);
}

fn wiped_fields() -> usize {
    WIPED_FIELDS.with(std::cell::Cell::get)
}

#[test]
fn partial_row_metadata_scope_and_digest_decode_use_wiping_field_guards() {
    // Static synthetic data only. No secret Value/String intermediate copies.
    for (input, minimum_wipes) in [
        (br#"{"binding_anchor_key":"allocated-anchor","advertised_models":["allocated-model"],"extra":0}"#.as_slice(), 2),
        (br#"{"binding_anchor_key":"allocated-anchor"}"#.as_slice(), 1),
        (br#"{"binding_anchor_key":"allocated-anchor","binding_anchor_key":"duplicate","advertised_models":[]}"#.as_slice(), 1),
        (br#"{"binding_anchor_key":"allocated-anchor","advertised_models":["allocated-model",5]}"#.as_slice(), 2),
    ] {
        let before = wiped_fields();
        assert!(serde_json::from_slice::<SecretMetadata>(input).is_err());
        assert!(wiped_fields() - before >= minimum_wipes);
    }
    for input in [
        br#"{"name":"allocated-box"}"#.as_slice(),
        br#"{"name":"allocated-box","epoch":"bad"}"#.as_slice(),
        br#"{"name":"allocated-box","epoch":1,"extra":0}"#.as_slice(),
    ] {
        let before = wiped_fields();
        assert!(serde_json::from_slice::<Scope>(input).is_err());
        assert!(wiped_fields() > before);
    }
    for input in [
        b"[1,2]".as_slice(),
        b"[1,2,999]".as_slice(),
        b"[1,2,\"bad\"]".as_slice(),
    ] {
        let before = wiped_fields();
        assert!(serde_json::from_slice::<SecretDigest>(input).is_err());
        assert!(wiped_fields() > before);
    }
    for input in [
        br#"{"metadata":{"binding_anchor_key":"allocated-anchor","advertised_models":["allocated-model"]},"box_scope":{"name":"allocated-box","epoch":1}}"#.as_slice(),
        br#"{"metadata":{"binding_anchor_key":"allocated-anchor","advertised_models":["allocated-model"]},"remaining_idle_nanos":"bad"}"#.as_slice(),
        br#"{"metadata":{"binding_anchor_key":"allocated-anchor","advertised_models":["allocated-model"]},"extra":0}"#.as_slice(),
    ] {
        let before = wiped_fields();
        assert!(serde_json::from_slice::<Row>(input).is_err());
        assert!(wiped_fields() - before >= 3);
    }
}

#[test]
fn dropping_prepared_grants_wipes_snapshot_without_creating_a_registry() {
    let f = Fixture::new();
    f.issue(6);
    let snapshot = f.snapshot();
    let before = wiped_fields();
    let prepared =
        LaunchTokenRegistry::prepare_handover_at(snapshot, &[], utc(), f.registry.0.clock.clone())
            .unwrap_or_else(|_| panic!("valid preparation refused"));
    assert_eq!(prepared.grants.len(), 1);
    assert_eq!(prepared.snapshot.rows[0].metadata, metadata());
    // Preparation has neither transferred nor cloned the guarded grant data.
    assert_eq!(wiped_fields(), before);
    drop(prepared);
    assert!(wiped_fields() - before >= 2);
    assert_eq!(f.registry.0.lock().len(), 1);
}

#[test]
fn source_activity_survives_prepare_delay_and_commit_without_new_activity() {
    let f = Fixture::new();
    let token = f.issue(4);
    f.advance(IDLE_TIMEOUT.as_secs() - 20);
    let snapshot = f.snapshot();
    let activity = snapshot.binding_activity().unwrap();
    let source = utc() - chrono::Duration::seconds(IDLE_TIMEOUT.as_secs() as i64 - 20);
    assert_eq!(activity[0].0.as_str(), "original-anchor:1");
    assert_eq!(activity[0].1, source);
    let prepared = LaunchTokenRegistry::prepare_handover_at(
        snapshot,
        &[],
        utc() + chrono::Duration::seconds(5),
        f.registry.0.clock.clone(),
    )
    .unwrap_or_else(|_| panic!("valid preparation refused"));
    // An arbitrarily delayed binding import consumes the already running lease.
    f.advance(15);
    let restored = prepared.commit();
    let digest = token_digest(token.expose_secret());
    assert_eq!(
        restored.0.lock()[&digest]
            .binding_activity
            .load(Ordering::Acquire),
        source.timestamp_millis()
    );
    assert_eq!(
        restored.get_or_pin(token.expose_secret()).unwrap_err(),
        LaunchTokenError::Expired
    );
}

#[test]
fn export_and_decode_reserve_row_capacity_before_first_sensitive_row() {
    let f = Fixture::new();
    f.issue(1);
    let snapshot = f.snapshot();
    assert!(snapshot.rows.capacity() >= CAPACITY);
    let bytes = snapshot.to_json().unwrap();
    let decoded = HandoverSnapshot::from_json(&bytes).unwrap();
    assert!(decoded.rows.capacity() >= CAPACITY);
    let prepared =
        LaunchTokenRegistry::prepare_handover_at(decoded, &[], utc(), f.registry.0.clock.clone())
            .unwrap_or_else(|_| panic!("valid preparation refused"));
    let restored = prepared.commit();
    assert_eq!(restored.0.lock().len(), 1);
}

#[test]
fn source_activity_before_unix_epoch_is_refused_before_staging() {
    let f = Fixture::new();
    f.issue(5);
    let mut snapshot = f.snapshot();
    snapshot.exported_at_utc = DateTime::from_timestamp(1, 0).unwrap();
    snapshot.rows[0].remaining_idle_nanos = 1;
    assert!(matches!(
        snapshot.binding_activity(),
        Err(HandoverError::Timestamp)
    ));
    assert!(matches!(
        LaunchTokenRegistry::prepare_handover_at(snapshot, &[], utc(), f.registry.0.clock.clone(),),
        Err(HandoverError::Timestamp)
    ));
}

#[test]
fn startup_convenience_restore_commits_only_a_prepared_grant() {
    let f = Fixture::new();
    let token = f.issue(8);
    let snapshot = f.registry.export_handover().unwrap();
    let restored = LaunchTokenRegistry::restore_handover(snapshot, &[]).unwrap();
    assert_eq!(
        restored
            .get_or_pin(token.expose_secret())
            .unwrap()
            .metadata(),
        metadata()
    );
}
