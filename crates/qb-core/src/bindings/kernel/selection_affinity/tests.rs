use super::*;
use chrono::Duration;

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap()
}

fn fixture() -> AffinityInput {
    let mut inputs = AffinityInput {
        bindings: BTreeMap::new(),
        groups: BTreeMap::new(),
        binding_count: 1,
        lru: vec![(now(), "claude:root".into())],
        active: BTreeSet::new(),
        pins: BTreeSet::new(),
        launch_retention: BTreeMap::new(),
        launch_anchors: BTreeMap::new(),
        session_binding_max_secs: 60,
    };
    add_group(&mut inputs, "claude:root", &["claude:root:agent:a"]);
    inputs
}

fn add_group(inputs: &mut AffinityInput, group: &str, keys: &[&str]) {
    inputs.groups.insert(
        group.into(),
        GroupInput {
            keys: keys.iter().map(|key| (*key).into()).collect(),
            account: format!("account:{group}"),
            last_used: now(),
        },
    );
    for key in keys {
        inputs.bindings.insert(
            (*key).into(),
            BindingInput {
                key: (*key).into(),
                account: format!("account:{group}"),
                last_used: now(),
                accepted: None,
            },
        );
    }
}

#[test]
fn idle_uses_group_time_whole_seconds_and_clamps_future() {
    let mut inputs = fixture();
    inputs.groups.get_mut("claude:root").unwrap().last_used =
        now() - Duration::milliseconds(59_999);
    // Binding time is metadata, not the group's idle clock.
    inputs.bindings.values_mut().next().unwrap().last_used = now() - Duration::days(7);
    assert!(retained(&inputs, "claude:root", now()).is_some());
    inputs.groups.get_mut("claude:root").unwrap().last_used = now() - Duration::seconds(60);
    assert!(retained(&inputs, "claude:root", now()).is_none());
    inputs.groups.get_mut("claude:root").unwrap().last_used = now() + Duration::days(1);
    assert!(retained(&inputs, "claude:root", now()).is_some());
    inputs.session_binding_max_secs = 0;
    assert!(retained(&inputs, "claude:root", now()).is_none());
}

#[test]
fn active_pin_and_launch_each_bypass_idle_and_capacity() {
    for protection in 0..3 {
        let mut inputs = fixture();
        inputs.session_binding_max_secs = 0;
        inputs.binding_count = CAPACITY + 1;
        match protection {
            0 => {
                inputs.active.insert("claude:root".into());
            }
            1 => {
                inputs.pins.insert("claude:root".into());
            }
            _ => {
                inputs.launch_retention.insert("claude:root".into(), now());
            }
        }
        assert!(retained(&inputs, "claude:root", now()).is_some());
    }
}

#[test]
fn launch_retention_boundary_and_future() {
    let mut inputs = fixture();
    inputs.session_binding_max_secs = 0;
    for (age_ms, expected) in [
        (IDLE_SECS as i64 * 1000 - 1, true),
        (IDLE_SECS as i64 * 1000, false),
        (-1000, true),
    ] {
        inputs
            .launch_retention
            .insert("claude:root".into(), now() - Duration::milliseconds(age_ms));
        assert_eq!(retained(&inputs, "claude:root", now()).is_some(), expected);
    }
}

#[test]
fn exact_then_first_supplied_member_preserves_metadata_and_order() {
    let mut inputs = fixture();
    add_group(&mut inputs, "opaque", &["z-hash", "a-hash"]);
    assert_eq!(retained(&inputs, "opaque", now()).unwrap().key, "z-hash");
    inputs.bindings.insert(
        "opaque".into(),
        BindingInput {
            key: "original-key".into(),
            account: "original-account".into(),
            last_used: now(),
            accepted: Some(false),
        },
    );
    let binding = retained(&inputs, "opaque", now()).unwrap();
    assert_eq!(
        (&*binding.key, &*binding.account),
        ("original-key", "original-account")
    );
    assert!(std::ptr::eq(binding, &inputs.bindings["opaque"]));
    inputs.bindings.remove("opaque");
    inputs.bindings.remove("z-hash");
    // Native fallback does not search later members when the first is absent.
    assert!(retained(&inputs, "opaque", now()).is_none());
    assert!(retained(&inputs, "missing", now()).is_none());
    assert!(retained(&inputs, "opaque:agent:x", now()).is_none());
}

#[test]
fn capacity_uses_total_count_order_and_group_cardinality() {
    let mut inputs = fixture();
    add_group(&mut inputs, "older", &["older-a", "older-b"]);
    inputs
        .lru
        .insert(0, (now() - Duration::seconds(1), "older".into()));
    // The supplied table is a scoped subset, so use binding_count, not map.len().
    inputs.binding_count = CAPACITY + 2;
    assert!(retained(&inputs, "claude:root", now()).is_some());
    inputs.binding_count = CAPACITY + 3;
    assert!(retained(&inputs, "claude:root", now()).is_none());
    inputs.binding_count = CAPACITY;
    assert!(retained(&inputs, "claude:root", now()).is_some());
    inputs.binding_count = CAPACITY + 1;
    inputs.launch_retention.insert("older".into(), now());
    assert!(retained(&inputs, "claude:root", now()).is_none());
    inputs
        .launch_retention
        .insert("older".into(), now() - Duration::seconds(IDLE_SECS as i64));
    assert!(retained(&inputs, "claude:root", now()).is_some());
}

#[test]
fn lru_only_skips_launch_retention_not_active_or_pin() {
    for pin in [false, true] {
        let mut inputs = fixture();
        add_group(&mut inputs, "older", &["older"]);
        inputs.lru.insert(0, (now(), "older".into()));
        inputs.binding_count = CAPACITY + 1;
        if pin {
            inputs.pins.insert("older".into());
        } else {
            inputs.active.insert("older".into());
        }
        assert!(retained(&inputs, "claude:root", now()).is_some());
    }
}

#[test]
fn source_anchor_precedes_ownership_and_acceptance() {
    let mut inputs = fixture();
    add_group(&mut inputs, "owner", &["owner"]);
    inputs
        .launch_anchors
        .insert("claude:root".into(), "owner".into());
    assert_eq!(
        source(&inputs, Some("claude:root:agent:a"), None, now()).as_deref(),
        Some("account:owner")
    );
    inputs.bindings.get_mut("owner").unwrap().accepted = Some(false);
    assert!(source(&inputs, Some("claude:root"), None, now()).is_none());
    assert_eq!(
        source(&inputs, Some("missing"), Some("policy-only"), now()).as_deref(),
        Some("policy-only")
    );
    assert_eq!(
        source(&inputs, Some("claude:root"), Some("policy-only"), now()).as_deref(),
        Some("policy-only")
    );
    assert!(source(&inputs, None, None, now()).is_none());
    for accepted in [None, Some(true)] {
        inputs.bindings.get_mut("owner").unwrap().accepted = accepted;
        assert_eq!(
            source(&inputs, Some("claude:root"), None, now()).as_deref(),
            Some("account:owner")
        );
    }
    inputs.groups.remove("owner");
    assert!(source(&inputs, Some("claude:root"), None, now()).is_none());
    inputs.launch_anchors.clear();
    assert_eq!(
        source(&inputs, Some("claude:root"), None, now()).as_deref(),
        Some("account:claude:root")
    );
}

#[test]
fn malformed_lru_fails_closed_without_panicking() {
    let mut inputs = fixture();
    inputs.binding_count = CAPACITY + 1;
    inputs.lru.insert(0, (now(), "omitted".into()));
    assert!(retained(&inputs, "claude:root", now()).is_none());
    inputs.groups.insert(
        "omitted".into(),
        GroupInput {
            keys: vec![String::new(); CAPACITY + 2],
            account: "omitted".into(),
            last_used: now(),
        },
    );
    assert!(retained(&inputs, "claude:root", now()).is_none());
}
