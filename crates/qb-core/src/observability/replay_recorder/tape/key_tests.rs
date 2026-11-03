//! Epoch-local replay needs the recorded readset, not an exported HMAC secret.
use super::*;
use crate::bindings::kernel::{self, CandidateInput, DecisionInputs, selection::SelectionInputs};
use serde_json::json;

const ACCOUNT: &str = "private-capacity-account";
const EMAIL: &str = "private-capacity-email@example.invalid";

fn at(n: u64) -> Stamp {
    Stamp {
        utc_ns: 1_000_000 + n as i64,
        monotonic_ns: n,
    }
}

fn inputs() -> DecisionInputs {
    use crate::accounts::{Account, Health, Identity, Source, TokenMeta, UserState};
    let now = "2026-10-30T00:00:00Z".parse().unwrap();
    let row = CandidateInput {
        account: Account {
            schema: 1,
            id: ACCOUNT.into(),
            provider: crate::accounts::Provider::Codex,
            label: "private-capacity-label".into(),
            email: EMAIL.into(),
            plan: Some("pro".into()),
            plan_observed_at: None,
            subscription: None,
            identity: Identity::Codex {
                account_id: "private-capacity-identity".into(),
                email: EMAIL.into(),
                user_id: None,
            },
            source: Source::Oauth,
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
        },
        inflight: 0,
        quota: None,
    };
    let mut selection: SelectionInputs = serde_json::from_value(json!({
        "entry": "ordered", "provider": "codex", "parent": null, "pinned": null,
        "hold_allowed": false, "preference": null, "root_hint": null,
        "candidates": [], "source_accounts": [], "account_order": [ACCOUNT],
        "affinity": {
            "bindings": {}, "groups": {}, "binding_count": 0, "lru": [],
            "active": [], "pins": [], "launch_retention": {}, "launch_anchors": {},
            "session_binding_max_secs": 60
        },
        "metrics": {
            "capacity": {(ACCOUNT): {"provider": "codex", "plan": "pro", "windows": {
                "5h": {"samples": [{"input_tokens": 1234, "used_pct_delta": 2.5,
                    "start_at": "2026-10-29T23:58:00Z", "end_at": "2026-10-29T23:59:00Z",
                    "reset_at": "2026-10-30T04:00:00Z"}]}
            }}},
            "config": crate::config::RoutingConfig::default(), "recent": {},
            "idle_placements": [], "thresholds": {}, "overload_marks": {},
            "pacing": {}, "placements": {}
        },
        "policy": {
            "scope": {"key": null, "exact": "private-policy-scope", "legacy": null},
            "defaults": {"mode": "recover", "reset_wait_secs": 0, "on_5h_limit": "move"},
            "limits": crate::routing::ChurnLimits::default(), "revision": 1,
            "state": {"sessions": {}, "legacy_codex_roots": []},
            "witness": {"sessions": {}, "legacy_codex_roots": []},
            "frozen": false, "history_overflow_until": null, "recovery_blocked": false,
            "lifecycle_recovery": false, "pool_moves": {},
            "target_pools": {(ACCOUNT): "private-capacity-pool"}
        },
        "launches": {}, "launch_aliases": [], "unknown_cost": [], "last_request_bytes": {},
        "outside_unknown": false, "outside_used": [], "entitled_targets": null,
        "return_guard": null
    }))
    .unwrap();
    selection.candidates.push(row.clone());
    selection.source_accounts.push(row.clone());
    DecisionInputs {
        now,
        model: None,
        key: None,
        from: None,
        to: String::new(),
        accounts: BTreeMap::from([(ACCOUNT.into(), row)]),
        traffic: BTreeMap::new(),
        pending: BTreeMap::new(),
        groups: BTreeMap::new(),
        bindings: BTreeMap::new(),
        accepted_unbound: BTreeSet::new(),
        sticky: BTreeMap::new(),
        outside_failure: false,
        permit: None,
        selection: Some(Box::new(selection)),
    }
}

fn capture(root: &Path) -> PathBuf {
    let (producer, writer) =
        open_with_provenance(root, Limits::default(), at(0), Provenance::Synthetic).unwrap();
    let path = writer.root.clone();
    let inputs = inputs();
    let actual = kernel::selection::select(&inputs).unwrap().decision;
    assert_eq!(
        producer.selection(
            at(1),
            1,
            crate::bindings::decision_tape::Entry::Chooser,
            1,
            1,
            &inputs,
            &actual
        ),
        Some(1)
    );
    writer.close(at(2)).unwrap();
    // No Producer/Writer or HMAC key is retained by the reader below.
    drop(producer);
    path
}

#[test]
fn capacity_readset_replays_after_key_drop_and_keeps_all_account_links() {
    let directory = super::tests::directory();
    let path = capture(&directory.path().join("tape"));
    let epoch = read_epoch(&path).unwrap();
    assert!(matches!(epoch.provenance, Provenance::Synthetic));
    assert_eq!(epoch.capture_started_utc_ns, at(0).utc_ns);
    assert_eq!(epoch.selections.len(), 1);
    let record = &epoch.selections[0].record;
    assert!(record.refusal.is_none());
    let restored = record.inputs.as_ref().unwrap().to_inputs().unwrap();
    let selection = restored.selection.as_ref().unwrap();
    let capacity = selection.metrics.capacity.as_ref().unwrap();
    assert_eq!(capacity.len(), 1);
    let opaque = &selection.account_order[0];
    assert_ne!(opaque, ACCOUNT);
    assert_eq!(opaque.len(), 34);
    assert!(opaque.starts_with("a:"));
    assert!(opaque[2..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(restored.accounts.contains_key(opaque));
    assert_eq!(&selection.candidates[0].account.id, opaque);
    assert_eq!(&selection.source_accounts[0].account.id, opaque);
    let expected = inputs();
    let expected_capacity = expected
        .selection
        .as_ref()
        .unwrap()
        .metrics
        .capacity
        .as_ref()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&capacity[opaque]).unwrap(),
        serde_json::to_value(&expected_capacity[ACCOUNT]).unwrap()
    );
    assert_eq!(
        permit_tape::selection::replay(record).unwrap(),
        record.actual
    );
    // This fixture has no initial witness or linked parent DecisionRecord.
    assert!(!epoch.selection_complete);
    let mut public = serde_json::to_string(&record).unwrap();
    public.push_str(&format!("{record:?}"));
    for entry in fs::read_dir(&path).unwrap() {
        let entry = entry.unwrap();
        let bytes = fs::read(entry.path()).unwrap();
        if entry.file_name() == segment_name(0).as_str() {
            public.push_str(
                &String::from_utf8(zstd::stream::decode_all(bytes.as_slice()).unwrap()).unwrap(),
            );
        } else {
            public.push_str(&String::from_utf8(bytes).unwrap());
        }
    }
    for canary in [
        ACCOUNT,
        EMAIL,
        "private-capacity-identity",
        "private-capacity-label",
        "private-policy-scope",
        "private-capacity-pool",
    ] {
        assert!(
            !public.contains(canary),
            "private canary reached archived/debug output"
        );
    }
    let header: serde_json::Value =
        serde_json::from_slice(&fs::read(path.join("header.json")).unwrap()).unwrap();
    let fields: BTreeSet<_> = header
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields,
        BTreeSet::from([
            "schema",
            "start",
            "capture",
            "identity_domain_version",
            "build",
            "provenance"
        ])
    );
}

#[test]
fn independent_epochs_do_not_require_or_share_identity_keys() {
    let directory = super::tests::directory();
    let first = read_epoch(&capture(&directory.path().join("first"))).unwrap();
    let second = read_epoch(&capture(&directory.path().join("second"))).unwrap();
    let account = |epoch: &crate::observability::decision_reader::Epoch| {
        epoch.selections[0]
            .record
            .inputs
            .as_ref()
            .unwrap()
            .to_inputs()
            .unwrap()
            .selection
            .unwrap()
            .account_order[0]
            .clone()
    };
    assert_ne!(account(&first), account(&second));
    assert_ne!(first.tape_sha256, second.tape_sha256);
    for epoch in [&first, &second] {
        let record = &epoch.selections[0].record;
        assert_eq!(
            permit_tape::selection::replay(record).unwrap(),
            record.actual
        );
    }
}

#[test]
fn reader_rejects_key_metadata_foreign_key_files_and_capture_mismatch() {
    let directory = super::tests::directory();
    let path = capture(&directory.path().join("tape"));
    let header_path = path.join("header.json");
    let original = fs::read(&header_path).unwrap();
    let hash = read_epoch(&path).unwrap().tape_sha256;
    for value in [
        serde_json::Value::Null,
        json!({"version": 1}),
        json!("malformed"),
    ] {
        let mut header: serde_json::Value = serde_json::from_slice(&original).unwrap();
        header["epoch_key"] = value;
        fs::write(&header_path, serde_json::to_vec(&header).unwrap()).unwrap();
        assert!(read_epoch(&path).is_err());
    }
    let mut header: serde_json::Value = serde_json::from_slice(&original).unwrap();
    header["capture"] = json!("foreign-epoch");
    fs::write(&header_path, serde_json::to_vec(&header).unwrap()).unwrap();
    assert!(read_epoch(&path).is_err());
    fs::write(&header_path, &original).unwrap();
    fs::write(path.join("epoch-key.json"), b"{}").unwrap();
    assert!(read_epoch(&path).is_err());
    fs::remove_file(path.join("epoch-key.json")).unwrap();
    // Absence of key material is valid and does not change the provenance hash.
    assert_eq!(read_epoch(&path).unwrap().tape_sha256, hash);
}
