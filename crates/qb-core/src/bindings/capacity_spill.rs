//! Process-local capacity-spill admission, serialized with binding publication.
use super::*;

pub(crate) struct CapacitySpillPermit {
    table: std::sync::Weak<BindingTable>,
    target: String,
    sequence: u64,
    previous: Option<u64>,
    committed: bool,
}

impl CapacitySpillPermit {
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for CapacitySpillPermit {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(table) = self.table.upgrade() {
            let mut d = table.data.lock().unwrap_or_else(|e| e.into_inner());
            if d.capacity_spills.get(&self.target) == Some(&self.sequence) {
                if let Some(previous) = self.previous {
                    d.capacity_spills.insert(self.target.clone(), previous);
                } else {
                    d.capacity_spills.remove(&self.target);
                }
            }
        }
    }
}

impl BindingTable {
    /// Startup attachment only. The owner can latch failure from a physical
    /// send without taking mutations, Data or Store in the opposite order.
    pub(crate) fn attach_outside_use_failure(&self, failure: Arc<AtomicBool>) {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outside_failure = Some(failure);
    }

    // Saved policy authority wins over an absent or stale write-behind image,
    // just as it does in native bound selection.
    pub(super) fn protected_source(
        &self,
        d: &Data,
        key: Option<&str>,
        anchor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<String> {
        anchor.map(str::to_owned).or_else(|| {
            key.and_then(|key| {
                self.retained_binding(d, launch::ownership_group(d, group(key)), now)
            })
            .filter(|binding| binding.accepted != Some(false))
            .map(|binding| binding.account.clone())
        })
    }

    /// Loss of causal state is a conservative destination veto, not attribution.
    pub(crate) fn set_outside_use_unknown(&self, unknown: bool) {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outside_unknown = unknown;
    }

    /// Publication shares the same gate as every native binding selection.
    pub(crate) fn set_outside_use(&self, account: &str, marked: bool) {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if marked {
            d.outside_used.insert(account.to_owned());
        } else {
            d.outside_used.remove(account);
        }
    }

    /// A preview only; admission rechecks the actual source under mutations.
    pub(crate) fn capacity_source(&self, key: Option<&str>) -> Option<String> {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let plan = self.policies.plan(launch::policy_key(&d, key));
        self.protected_source(&d, key, plan.anchor.as_deref(), Utc::now())
    }

    /// Called from the selector's admission callback while mutations is held.
    /// Never treats a timer, cached candidate or reset as fresh usage evidence.
    pub(crate) fn admit_capacity_spill(
        self: &Arc<Self>,
        key: Option<&str>,
        source: &str,
        target: &str,
    ) -> std::result::Result<CapacitySpillPermit, ()> {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let plan = self.policies.plan(launch::policy_key(&d, key));
        if source == target
            || self
                .protected_source(&d, key, plan.anchor.as_deref(), Utc::now())
                .as_deref()
                != Some(source)
        {
            return Err(());
        }
        if let Some(spill) = d.capacity_spills.get(target)
            && !d.placements.get(target).is_some_and(|state| {
                !state.windows.is_empty()
                    && state.windows.values().all(|window| {
                        window.asof.is_some_and(|asof| {
                            asof.sequence > *spill
                                && state.latest.is_none_or(|placed| asof.sequence > placed)
                        })
                    })
            })
        {
            return Err(());
        }
        let sequence = self.begin_usage_observation();
        let previous = d.capacity_spills.insert(target.to_owned(), sequence);
        Ok(CapacitySpillPermit {
            table: Arc::downgrade(self),
            target: target.to_owned(),
            sequence,
            previous,
            committed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, Arc<BindingTable>) {
        let root = super::super::tests::fixture_dir();
        let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
        for index in 0..4 {
            let key = format!("codex:spill-{index}");
            table.data.lock().unwrap().bindings.insert(
                key.clone(),
                Binding::new(
                    key.clone(),
                    "source".into(),
                    Utc::now(),
                    Some(true),
                    None,
                    None,
                    None,
                    true,
                ),
            );
            table.data.lock().unwrap().groups.insert(
                key.clone(),
                Group {
                    keys: BTreeSet::from([key]),
                    account: "source".into(),
                    last_used: Utc::now(),
                },
            );
        }
        (root, table)
    }

    fn snapshot(names: &[&str]) -> Value {
        json!({"windows": names.iter().map(|name| json!({
            "name": name, "used_pct": 0.0,
            "window_minutes": if *name == "5h" { 300 } else { 10080 },
            "reset_at": Utc::now() + chrono::Duration::hours(5),
            "observed_at": Utc::now()
        })).collect::<Vec<_>>()})
    }

    #[test]
    fn n10_four_distinct_spills_admit_only_one_before_fresh_usage() {
        let (_root, table) = fixture();
        let old = table.begin_usage_observation_for("reset-target");
        table.note_usage_observation_asof_token("reset-target", &snapshot(&["5h", "7d"]), old);
        let first = table
            .admit_capacity_spill(Some("codex:spill-0"), "source", "reset-target")
            .unwrap();
        first.commit();
        for index in 1..4 {
            assert!(
                table
                    .admit_capacity_spill(
                        Some(&format!("codex:spill-{index}")),
                        "source",
                        "reset-target"
                    )
                    .is_err()
            );
        }
        // Cached zero usage and a later reset timestamp alone do not reopen it.
        table.note_usage_observation_asof_token("reset-target", &snapshot(&["5h", "7d"]), old);
        assert!(
            table
                .admit_capacity_spill(Some("codex:spill-1"), "source", "reset-target")
                .is_err()
        );
        let fresh = table.begin_usage_observation_for("reset-target");
        table.note_usage_observation_asof_token("reset-target", &snapshot(&["5h"]), fresh);
        assert!(
            table
                .admit_capacity_spill(Some("codex:spill-1"), "source", "reset-target")
                .is_err(),
            "partial coverage cannot acknowledge the weekly window"
        );
        table.note_usage_observation_asof_token("reset-target", &snapshot(&["7d"]), fresh);
        table
            .admit_capacity_spill(Some("codex:spill-1"), "source", "reset-target")
            .unwrap()
            .commit();
        assert!(
            table
                .admit_capacity_spill(Some("codex:spill-2"), "source", "reset-target")
                .is_err()
        );
    }

    #[test]
    fn n10_failed_admission_rolls_back_only_its_own_reservation() {
        let (_root, table) = fixture();
        let first = table
            .admit_capacity_spill(Some("codex:spill-0"), "source", "target")
            .unwrap();
        assert!(
            table
                .admit_capacity_spill(Some("codex:spill-1"), "source", "target")
                .is_err()
        );
        drop(first);
        table
            .admit_capacity_spill(Some("codex:spill-1"), "source", "target")
            .unwrap()
            .commit();
        assert!(
            table
                .admit_capacity_spill(Some("codex:spill-2"), "source", "target")
                .is_err()
        );
    }

    #[test]
    fn n10_spill_rechecks_source_and_requires_a_real_bound_session() {
        let (_root, table) = fixture();
        assert_eq!(
            table.capacity_source(Some("codex:spill-0")).as_deref(),
            Some("source")
        );
        assert!(
            table
                .admit_capacity_spill(Some("codex:spill-0"), "obsolete", "target")
                .is_err()
        );
        assert!(
            table
                .admit_capacity_spill(Some("codex:new"), "source", "target")
                .is_err()
        );
        assert!(
            table
                .admit_capacity_spill(None, "source", "target")
                .is_err()
        );
        assert!(
            table
                .admit_capacity_spill(Some("codex:spill-0"), "source", "source")
                .is_err()
        );
    }

    #[test]
    fn n10_usage_started_before_spill_publication_does_not_acknowledge_it() {
        let (_root, table) = fixture();
        let permit = table
            .admit_capacity_spill(Some("codex:spill-0"), "source", "target")
            .unwrap();
        let early = table.begin_usage_observation_for("target");
        // Actual placement follows policy publication, after the admission seam.
        {
            let sequence = table.begin_usage_observation();
            table
                .data
                .lock()
                .unwrap()
                .placements
                .entry("target".into())
                .or_default()
                .placed(Utc::now(), sequence);
        }
        permit.commit();
        table.note_usage_observation_asof_token("target", &snapshot(&["5h", "7d"]), early);
        assert!(
            table
                .admit_capacity_spill(Some("codex:spill-1"), "source", "target")
                .is_err()
        );
        let later = table.begin_usage_observation_for("target");
        table.note_usage_observation_asof_token("target", &snapshot(&["5h", "7d"]), later);
        assert!(
            table
                .admit_capacity_spill(Some("codex:spill-1"), "source", "target")
                .is_ok()
        );
    }

    #[test]
    fn n10_failed_replacement_retains_the_previous_committed_spill_fence() {
        let (_root, table) = fixture();
        table
            .admit_capacity_spill(Some("codex:spill-0"), "source", "target")
            .unwrap()
            .commit();
        let first_marker = table.data.lock().unwrap().capacity_spills["target"];
        let fresh = table.begin_usage_observation_for("target");
        table.note_usage_observation_asof_token("target", &snapshot(&["5h", "7d"]), fresh);
        let denied = table
            .admit_capacity_spill(Some("codex:spill-1"), "source", "target")
            .unwrap();
        drop(denied);
        assert_eq!(
            table
                .data
                .lock()
                .unwrap()
                .capacity_spills
                .get("target")
                .copied(),
            Some(first_marker),
            "failed replacement must retain the committed spill freshness fence"
        );
        let later_placement = table.begin_usage_observation();
        table
            .data
            .lock()
            .unwrap()
            .placements
            .get_mut("target")
            .unwrap()
            .placed(Utc::now(), later_placement);
        assert!(
            table
                .admit_capacity_spill(Some("codex:spill-2"), "source", "target")
                .is_err(),
            "a failed replacement must not erase the old source of freshness authority"
        );
        let latest = table.begin_usage_observation_for("target");
        table.note_usage_observation_asof_token("target", &snapshot(&["5h", "7d"]), latest);
        table
            .admit_capacity_spill(Some("codex:spill-2"), "source", "target")
            .unwrap()
            .commit();
        assert!(
            table
                .admit_capacity_spill(Some("codex:spill-3"), "source", "target")
                .is_err()
        );
    }

    #[test]
    fn n10_native_spill_publication_takes_the_ordinary_d20_placement_charge() {
        let (_root, table) = fixture();
        let mut target = super::super::tests::candidate("target", 0.0, 24, 5);
        target.account.provider = Provider::Codex;
        let mut reserved = None;
        let decision = table.choose_with_hold_at_admitted(
            Provider::Codex,
            Some("codex:spill-0"),
            None,
            None,
            &[target],
            None,
            42,
            false,
            false,
            Utc::now(),
            |account| {
                reserved = table
                    .admit_capacity_spill(Some("codex:spill-0"), "source", account)
                    .ok();
                reserved.is_some()
            },
        );
        assert!(matches!(decision, Decision::Selected { .. }));
        reserved
            .expect("native selector must reserve the spill")
            .commit();
        let d = table.data.lock().unwrap();
        let placement = &d.placements["target"];
        assert_eq!(placement.total, 1, "a forced capacity move still pays D20");
        assert!(placement.latest.unwrap() > d.capacity_spills["target"]);
    }

    #[test]
    fn n10_lost_coverage_protects_future_accounts_without_inventing_attribution() {
        let (_root, table) = fixture();
        table.set_outside_use_unknown(true);
        let mut newly_added = super::super::tests::candidate("new-target", 0.0, 24, 5);
        newly_added.account.provider = Provider::Codex;
        assert!(matches!(
            table.choose(
                Provider::Codex,
                Some("codex:spill-0"),
                None,
                None,
                std::slice::from_ref(&newly_added),
                None,
                42
            ),
            Decision::None
        ));
        assert!(matches!(
            table.choose(
                Provider::Codex,
                Some("codex:genuinely-new"),
                None,
                None,
                &[newly_added],
                None,
                42
            ),
            Decision::Selected { .. }
        ));
        assert!(
            table.data.lock().unwrap().outside_used.is_empty(),
            "unknown coverage is not a positive outside-use observation"
        );
    }

    #[test]
    fn n10_durable_anchor_protects_without_a_binding_and_overrides_stale_source() {
        for stale in [false, true] {
            let (root, table) = fixture();
            let key = "codex:spill-0";
            table
                .policies
                .set_session(
                    &crate::routing::session_id(key),
                    Some(crate::routing::Settings {
                        mode: crate::routing::Mode::Recover,
                        ..Default::default()
                    }),
                    None,
                    Some("source"),
                )
                .unwrap();
            // Reload persisted policy authority without a binding image.
            drop(table);
            let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
            if stale {
                let mut d = table.data.lock().unwrap();
                d.bindings.insert(
                    key.into(),
                    Binding::new(
                        key.into(),
                        "target".into(),
                        Utc::now(),
                        Some(true),
                        None,
                        None,
                        None,
                        false,
                    ),
                );
                d.groups.insert(
                    key.into(),
                    Group {
                        keys: BTreeSet::from([key.into()]),
                        account: "target".into(),
                        last_used: Utc::now(),
                    },
                );
                assert_eq!(
                    table
                        .retained_binding(&d, key, Utc::now())
                        .map(|binding| binding.account.as_str()),
                    Some("target"),
                    "the stale control must be a genuinely retained indexed binding"
                );
            }
            table.set_outside_use("target", true);
            assert_eq!(table.capacity_source(Some(key)).as_deref(), Some("source"));
            assert!(
                table
                    .admit_capacity_spill(Some(key), "target", "other")
                    .is_err()
            );
            table
                .admit_capacity_spill(Some(key), "source", "other")
                .unwrap()
                .commit();
            let mut target = super::super::tests::candidate("target", 0.0, 24, 5);
            target.account.provider = Provider::Codex;
            assert!(
                matches!(
                    table.choose(
                        Provider::Codex,
                        Some(key),
                        None,
                        None,
                        std::slice::from_ref(&target),
                        None,
                        42
                    ),
                    Decision::None
                ),
                "a persisted existing session must not move to an outside-used destination"
            );
            assert_eq!(
                table.policies.plan(Some(key)).anchor.as_deref(),
                Some("source")
            );
            assert!(matches!(
                table.choose(
                    Provider::Codex,
                    Some("codex:new"),
                    None,
                    None,
                    &[target],
                    None,
                    42
                ),
                Decision::Selected { .. }
            ));
            let mut source = super::super::tests::candidate("source", 0.0, 24, 5);
            source.account.provider = Provider::Codex;
            table.set_outside_use("source", true);
            assert!(matches!(
                table.choose(Provider::Codex, Some(key), None, None, &[source], None, 42),
                Decision::Selected { .. }
            ));
        }
    }
}
