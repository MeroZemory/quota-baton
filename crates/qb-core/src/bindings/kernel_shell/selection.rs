//! Snapshot the actual selection transaction after its native shell mutations.
//! Scope contains owner rows and identity/order relations, never scored answers.
use super::super::*;
use kernel::selection::{PreferenceInput, RootHintInput, SelectionInputs};
use kernel::selection_affinity::{AffinityInput, BindingInput, GroupInput};
use kernel::selection_metrics::{
    MetricInputs, OverloadInput, PlacementAsOfInput, PlacementInput, PlacementWindowInput,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::bindings) enum SnapshotRefusal {
    RootHint,
    Clock,
}

#[allow(clippy::too_many_arguments)]
pub(in crate::bindings) fn snapshot(
    table: &BindingTable,
    d: &Data,
    entry: kernel::selection::SelectionEntry,
    provider: Provider,
    key: Option<&str>,
    parent: Option<&str>,
    model: Option<&str>,
    candidates: &[Candidate],
    sources: &[Candidate],
    entitled_targets: Option<&BTreeSet<String>>,
    observations: MoveObservations,
    pinned: Option<&str>,
    now: DateTime<Utc>,
    hold_allowed: bool,
    preference: Option<&launch::SelectionPreference<'_>>,
    root_hint: Option<&crate::boxes::RootHint>,
) -> Result<kernel::DecisionInputs, SnapshotRefusal> {
    let row = |c: &Candidate| kernel::CandidateInput {
        account: c.account.clone(),
        quota: c.quota.clone(),
        inflight: c.inflight,
    };
    let root_hint = root_hint
        .map(|hint| {
            [Provider::Claude, Provider::Codex]
                .into_iter()
                .find_map(|provider| {
                    hint.for_provider(provider).map(|key| RootHintInput {
                        provider,
                        key: key.to_owned(),
                    })
                })
                .ok_or(SnapshotRefusal::RootHint)
        })
        .transpose()?;
    let mut queries: BTreeSet<String> = key
        .into_iter()
        .chain(parent)
        .chain(root_hint.as_ref().map(|hint| hint.key.as_str()))
        .map(str::to_owned)
        .collect();
    for query in queries.clone() {
        let root = group(&query);
        queries.insert(root.to_owned());
        queries.insert(launch::ownership_group(d, root).to_owned());
    }
    let group_queries: BTreeSet<_> = queries.iter().map(|key| group(key).to_owned()).collect();
    let mut affinity_groups = group_queries.clone();
    // Below capacity the native read does not traverse unrelated LRU rows.
    let lru: Vec<_> = if d.bindings.len() > CAPACITY {
        d.lru.iter().cloned().collect()
    } else {
        Vec::new()
    };
    affinity_groups.extend(lru.iter().map(|(_, key)| key.clone()));
    let groups: BTreeMap<_, _> = affinity_groups
        .iter()
        .filter_map(|key| {
            d.groups.get(key).map(|entry| {
                (
                    key.clone(),
                    GroupInput {
                        keys: entry.keys.iter().cloned().collect(),
                        account: entry.account.clone(),
                        last_used: entry.last_used,
                    },
                )
            })
        })
        .collect();
    let binding_queries: BTreeSet<_> = queries
        .iter()
        .cloned()
        .chain(
            groups
                .iter()
                .filter(|(key, _)| group_queries.contains(*key))
                .flat_map(|(_, entry)| entry.keys.iter().cloned()),
        )
        .collect();
    let bindings = binding_queries
        .iter()
        .filter_map(|key| {
            d.bindings.get(key).map(|binding| {
                (
                    key.clone(),
                    BindingInput {
                        key: binding.key.clone(),
                        account: binding.account.clone(),
                        last_used: binding.last_used,
                        accepted: binding.accepted,
                    },
                )
            })
        })
        .collect();
    let cohort: BTreeSet<_> = candidates
        .iter()
        .chain(sources)
        .map(|candidate| candidate.account.id.clone())
        .collect();
    let policy = table.policies.selection_inputs(
        launch::policy_key(d, key),
        &cohort.iter().cloned().collect::<Vec<_>>(),
    );
    let mut activity_accounts = cohort.clone();
    activity_accounts.extend(groups.values().map(|entry| entry.account.clone()));
    activity_accounts.extend(
        policy
            .state
            .sessions
            .values()
            .chain(policy.witness.sessions.values())
            .filter_map(|session| session.anchor.clone()),
    );
    activity_accounts.extend(d.sticky.get(provider.as_str()).cloned());
    let overload_marks: BTreeMap<_, _> = d
        .overload_marks
        .iter()
        .filter(|(key, mark)| {
            activity_accounts.contains(&mark.target) || group_queries.contains(*key)
        })
        .map(|(key, mark)| {
            (
                key.clone(),
                OverloadInput {
                    expires: mark.expires,
                    from: mark.from.clone(),
                    target: mark.target.clone(),
                },
            )
        })
        .collect();
    for mark in overload_marks.values() {
        activity_accounts.insert(mark.from.clone());
        activity_accounts.insert(mark.target.clone());
    }
    let recent: BTreeMap<_, _> = activity_accounts
        .iter()
        .filter_map(|id| {
            d.accounts
                .get(id)
                .map(|index| (id.clone(), index.recent.iter().cloned().collect::<Vec<_>>()))
        })
        .collect();
    let mut activity_groups = affinity_groups.clone();
    activity_groups.extend(recent.values().flatten().map(|(_, key)| key.clone()));
    let active = d
        .active
        .keys()
        .filter(|key| activity_groups.contains(*key))
        .cloned()
        .collect();
    let pins = d
        .pins
        .keys()
        .filter(|key| affinity_groups.contains(*key))
        .cloned()
        .collect();
    let placements = cohort
        .iter()
        .filter_map(|id| {
            d.placements.get(id).map(|state| {
                (
                    id.clone(),
                    PlacementInput {
                        latest: state.latest,
                        total: state.total,
                        window_set_sequence: state.window_set_sequence,
                        windows: state
                            .windows
                            .iter()
                            .map(|(name, window)| {
                                (
                                    name.clone(),
                                    PlacementWindowInput {
                                        value: window.value.clone(),
                                        asof: window.asof.map(|token| PlacementAsOfInput {
                                            sequence: token.sequence,
                                            placed_count: token.placed_count,
                                        }),
                                        reported_sequence: window.reported_sequence,
                                        unresolved_equal_charge: window.unresolved_equal_charge,
                                    },
                                )
                            })
                            .collect(),
                    },
                )
            })
        })
        .collect();
    let guard_key = key
        .map(group)
        .map(|key| launch::ownership_group(d, key).to_owned())
        .unwrap_or_else(|| format!("unidentified:{}", provider.as_str()));
    let return_guard = if entry == kernel::selection::SelectionEntry::ReturnGuard {
        Some(
            table
                .move_guard
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .selection_return_inputs(&guard_key, observations)
                .ok_or(SnapshotRefusal::Clock)?,
        )
    } else {
        None
    };
    let mut accounts = BTreeMap::new();
    for candidate in candidates.iter().chain(sources) {
        accounts
            .entry(candidate.account.id.clone())
            .or_insert_with(|| row(candidate));
    }
    let selection = SelectionInputs {
        entry,
        provider,
        parent: parent.map(str::to_owned),
        pinned: pinned.map(str::to_owned),
        hold_allowed,
        preference: preference.map(|p| PreferenceInput {
            account: p.account.map(str::to_owned),
            coverage: p.coverage.cloned(),
            model_move: p.model_move,
        }),
        root_hint,
        candidates: candidates.iter().map(row).collect(),
        source_accounts: sources.iter().map(row).collect(),
        account_order: cohort.iter().cloned().collect(),
        affinity: AffinityInput {
            bindings,
            groups,
            binding_count: d.bindings.len(),
            lru,
            active,
            pins,
            launch_retention: d
                .launch_retention
                .iter()
                .filter(|(key, _)| affinity_groups.contains(*key))
                .map(|(key, at)| (key.clone(), *at))
                .collect(),
            launch_anchors: d
                .launch_anchors
                .keys()
                .filter(|key| queries.contains(*key))
                .map(|key| (key.clone(), launch::ownership_group(d, key).to_owned()))
                .collect(),
            session_binding_max_secs: table.config.session_binding_max_secs,
        },
        metrics: MetricInputs {
            config: table.config.clone(),
            recent,
            idle_placements: activity_groups
                .iter()
                .filter(|key| launch::idle_placement(d, key))
                .cloned()
                .collect(),
            thresholds: table
                .thresholds
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .filter(|(id, _)| activity_accounts.contains(*id))
                .map(|(id, value)| (id.clone(), *value))
                .collect(),
            overload_marks,
            pacing: d
                .pacing
                .iter()
                .filter(|(id, _)| cohort.contains(*id))
                .map(|(id, value)| (id.clone(), *value))
                .collect(),
            placements,
        },
        policy,
        launches: d
            .launches
            .keys()
            .filter(|key| queries.contains(*key))
            .filter_map(|key| launch::selection_metadata(d, key).map(|row| (key.clone(), row)))
            .collect(),
        launch_aliases: d
            .launch_aliases
            .keys()
            .filter(|key| queries.contains(*key))
            .cloned()
            .collect(),
        unknown_cost: d
            .unknown_cost
            .iter()
            .filter(|key| queries.contains(*key))
            .cloned()
            .collect(),
        last_request_bytes: d
            .last_request_bytes
            .iter()
            .filter(|(key, _)| queries.contains(*key))
            .map(|(key, value)| (key.clone(), *value))
            .collect(),
        outside_unknown: d.outside_unknown,
        outside_used: d.outside_used.intersection(&cohort).cloned().collect(),
        entitled_targets: entitled_targets.cloned(),
        return_guard,
    };
    Ok(kernel::DecisionInputs {
        now,
        model: model.map(str::to_owned),
        key: key.map(str::to_owned),
        // Permit-only context is not read by the selection kernel or archived
        // in its wire protocol. Actual source/target are derived from raw rows.
        from: None,
        to: String::new(),
        accounts,
        traffic: d
            .traffic
            .iter()
            .filter(|(id, _)| cohort.contains(*id))
            .map(|(id, at)| (id.clone(), *at))
            .collect(),
        pending: d
            .pending
            .iter()
            .filter(|(id, _)| cohort.contains(*id))
            .map(|(id, value)| (id.clone(), *value))
            .collect(),
        groups: d
            .groups
            .iter()
            .filter(|(key, _)| group_queries.contains(*key))
            .map(|(key, entry)| {
                (
                    key.clone(),
                    kernel::AcceptanceGroup {
                        account: entry.account.clone(),
                        keys: entry.keys.clone(),
                    },
                )
            })
            .collect(),
        bindings: binding_queries
            .iter()
            .filter_map(|key| {
                d.bindings.get(key).map(|entry| {
                    (
                        key.clone(),
                        kernel::AcceptanceBinding {
                            accepted: entry.accepted,
                        },
                    )
                })
            })
            .collect(),
        accepted_unbound: d.accepted_unbound.intersection(&cohort).cloned().collect(),
        sticky: d.sticky.clone(),
        outside_failure: d
            .outside_failure
            .as_ref()
            .is_some_and(|failed| failed.load(Ordering::Acquire)),
        permit: None,
        selection: Some(Box::new(selection)),
    })
}
