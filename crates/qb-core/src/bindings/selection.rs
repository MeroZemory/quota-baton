//! C7 selection owns discretionary move evidence. Reason labels are output only.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionCode {
    New,
    Forced,
    Inherited,
    MarkWait,
    ColdRebalance,
    Overload,
}
impl SelectionCode {
    fn code(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Forced => "forced",
            Self::Inherited => "inherited",
            Self::MarkWait => "mark_wait",
            Self::ColdRebalance => "cold_rebalance",
            Self::Overload => "overload",
        }
    }
}

/// Only this selection owner can construct a cold/overload judgement. It is
/// consumed in the same mutations transaction that evaluated all C7 guards.
pub(super) struct RebalanceEvidence {
    kind: SelectionCode,
    key: String,
    from: String,
    to: String,
    model: Option<String>,
    at: DateTime<Utc>,
}
impl RebalanceEvidence {
    pub(super) fn label(&self) -> &'static str {
        self.kind.code()
    }
    pub(super) fn applies_to(
        &self,
        key: Option<&str>,
        from: &str,
        to: &str,
        model: Option<&str>,
        at: DateTime<Utc>,
    ) -> bool {
        key.is_some_and(|key| group(key) == self.key)
            && self.from == from
            && self.to == to
            && self.model.as_deref() == model
            && self.at == at
    }
}

pub(super) struct SelectionReason {
    code: SelectionCode,
    evidence: Option<RebalanceEvidence>,
}
impl SelectionReason {
    pub(super) fn evidence(&self) -> Option<&RebalanceEvidence> {
        self.evidence.as_ref()
    }
    pub(super) fn code(&self) -> &'static str {
        self.code.code()
    }
    pub(super) fn valid_for(
        &self,
        key: Option<&str>,
        from: Option<&str>,
        to: &str,
        model: Option<&str>,
        at: DateTime<Utc>,
    ) -> bool {
        // Keeping a source after a breaker veto never spends a move authority.
        if from == Some(to) {
            return true;
        }
        if matches!(
            self.code,
            SelectionCode::ColdRebalance | SelectionCode::Overload
        ) {
            from.is_some_and(|from| {
                self.evidence
                    .as_ref()
                    .is_some_and(|proof| proof.applies_to(key, from, to, model, at))
            })
        } else {
            true
        }
    }
}
/// Created only by selection under mutations, from policy/entitlement-filtered full
/// candidates. A caller's pin or a raw account count cannot create this fact.
pub(super) struct SoleEligibleTarget {
    from: String,
    to: String,
    model: Option<String>,
    at: DateTime<Utc>,
}
impl SoleEligibleTarget {
    pub(super) fn applies_to(
        &self,
        from: &str,
        to: &str,
        model: Option<&str>,
        at: DateTime<Utc>,
    ) -> bool {
        self.from == from && self.to == to && self.model.as_deref() == model && self.at == at
    }
}

impl BindingTable {
    /// Preserve C7 ranking, but try non-return alternatives before declaring a
    /// newly unusable source stranded. Admission/policy commit still run once.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn select_with_return_guard(
        &self,
        d: &Data,
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
    ) -> (PreviewDecision, SelectionReason, Option<SoleEligibleTarget>) {
        let selected = self.select_ordered(
            d,
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            now,
            hold_allowed,
            preference,
        );
        let PreviewDecision::Selected { account } = &selected.0 else {
            return (selected.0, selected.1, None);
        };
        let Some(from) = self.rebind_budget_source(d, provider, key, account) else {
            return (selected.0, selected.1, None);
        };
        let Some(source) = sources.iter().find(|c| c.account.id == from) else {
            return (selected.0, selected.1, None);
        };
        let source = effective_account(source, d, &self.config, model, now);
        if crate::accounts::health::UnusableEvidence::observe(
            &source,
            model,
            Self::accepted_in(d, key, &from),
            now,
        )
        .is_none()
        {
            return (selected.0, selected.1, None);
        }
        let policy_key = launch::policy_key(d, key);
        let outside_unknown = d.outside_unknown
            || d.outside_failure
                .as_ref()
                .is_some_and(|failed| failed.load(Ordering::Acquire));
        // Full health-owner evidence proves absence of another target; retry or
        // transport exclusion alone must never manufacture sole eligibility.
        let viable: BTreeSet<_> = sources
            .iter()
            .filter(|c| {
                c.account.id != from
                    && c.account.provider == provider
                    && !c.account.drain
                    && effective_account(c, d, &self.config, model, now).eligible_at(model, now)
                    && !outside_unknown
                    && !d.outside_used.contains(&c.account.id)
                    && entitled_targets.is_none_or(|ids| ids.contains(&c.account.id))
                    && self.policies.permits_dispatch(policy_key, &c.account.id)
            })
            .map(|c| c.account.id.as_str())
            .collect();
        if viable.len() == 1 && viable.contains(account.as_str()) {
            let proof = SoleEligibleTarget {
                from,
                to: account.clone(),
                model: model.map(str::to_owned),
                at: now,
            };
            return (selected.0, selected.1, Some(proof));
        }
        let guard_key = key
            .map(group)
            .map(|key| launch::ownership_group(d, key).to_owned())
            .unwrap_or_else(|| format!("unidentified:{}", provider.as_str()));
        let guard = self.move_guard.lock().unwrap_or_else(|e| e.into_inner());
        let alternatives: Vec<_> = candidates
            .iter()
            .filter(|c| viable.contains(c.account.id.as_str()))
            .filter(|c| {
                !guard.return_blocked(
                    &guard_key,
                    &c.account.id,
                    semantic_evidence(Some(&effective_account(c, d, &self.config, model, now))),
                    observations,
                    now,
                )
            })
            .cloned()
            .collect();
        let selected_blocked = candidates
            .iter()
            .find(|c| c.account.id == *account)
            .is_some_and(|c| {
                guard.return_blocked(
                    &guard_key,
                    account,
                    semantic_evidence(Some(&effective_account(c, d, &self.config, model, now))),
                    observations,
                    now,
                )
            });
        drop(guard);
        if selected_blocked && !alternatives.is_empty() {
            let alternative = self.select_ordered(
                d,
                provider,
                key,
                parent,
                model,
                &alternatives,
                pinned,
                now,
                hold_allowed,
                preference,
            );
            return (alternative.0, alternative.1, None);
        }
        (selected.0, selected.1, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn select_ordered(
        &self,
        d: &Data,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        now: DateTime<Utc>,
        hold_allowed: bool,
        preference: Option<&launch::SelectionPreference<'_>>,
    ) -> (PreviewDecision, SelectionReason) {
        let plan = self.policies.plan(launch::policy_key(d, key));
        let (choice, code) = self.select_ranked(
            d,
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            now,
            hold_allowed,
            preference,
            &plan,
        );
        let evidence = if matches!(code, SelectionCode::ColdRebalance | SelectionCode::Overload) {
            match (&choice, key) {
                (PreviewDecision::Selected { account }, Some(key)) => {
                    // This is the source actually evaluated by select_ranked.
                    // A retained, not-yet-accepted row can also become cold;
                    // WS protection's accepted-only filter is not its identity.
                    plan.anchor
                        .clone()
                        .or_else(|| {
                            self.retained_binding(d, key, now)
                                .map(|b| b.account.clone())
                        })
                        .filter(|from| from != account)
                        .map(|from| RebalanceEvidence {
                            kind: code,
                            key: group(key).to_owned(),
                            from,
                            to: account.clone(),
                            model: model.map(str::to_owned),
                            at: now,
                        })
                }
                _ => None,
            }
        } else {
            None
        };
        (choice, SelectionReason { code, evidence })
    }
    #[allow(clippy::too_many_arguments)]
    fn select_ranked(
        &self,
        d: &Data,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        now: DateTime<Utc>,
        hold_allowed: bool,
        preference: Option<&launch::SelectionPreference<'_>>,
        plan: &crate::routing::Plan,
    ) -> (PreviewDecision, SelectionCode) {
        let policy_key = launch::policy_key(d, key);
        let model_move = preference.is_some_and(|p| p.model_move);
        let strict_anchor = plan.anchor.clone().or_else(|| {
            key.and_then(|k| self.retained_binding(d, k, now))
                .map(|b| b.account.clone())
        });
        let source = self.protected_source(d, key, plan.anchor.as_deref(), now);
        let outside_unknown = d.outside_unknown
            || d.outside_failure
                .as_ref()
                .is_some_and(|failed| failed.load(Ordering::Acquire));
        let mut entries: Vec<_> = candidates
            .iter()
            .filter(|c| c.account.provider == provider)
            .filter(|c| {
                model_move
                    || plan.settings.mode != crate::routing::Mode::Strict
                    || strict_anchor
                        .as_ref()
                        .is_none_or(|anchor| anchor == &c.account.id)
            })
            .map(|c| {
                let eligible = effective_account(c, d, &self.config, model, now)
                    .eligible_at(model, now)
                    && (!c.account.drain || Self::accepted_in(d, key, &c.account.id));
                (c, eligible)
            })
            .collect();
        if let Some(source) = source.as_deref() {
            // Outside use never displaces an existing source. It only vetoes
            // destinations, including forced moves and explicit preferences.
            for (candidate, eligible) in &mut entries {
                *eligible &= candidate.account.id == source
                    || (!outside_unknown && !d.outside_used.contains(&candidate.account.id));
            }
        } else if entries.iter().any(|(candidate, eligible)| {
            *eligible && !outside_unknown && !d.outside_used.contains(&candidate.account.id)
        }) {
            // Last resort is before C7 preference/gate partitions: a healthy
            // unmarked account cannot be bypassed by a reset or plan ranking.
            entries.retain(|(candidate, _)| !d.outside_used.contains(&candidate.account.id));
        }
        if let Some(id) = pinned
            && entries
                .iter()
                .any(|(c, eligible)| c.account.id == id && *eligible)
        {
            return (
                PreviewDecision::Selected { account: id.into() },
                SelectionCode::Forced,
            );
        }
        struct SelectionBinding {
            key: String,
            account: String,
            last_used: DateTime<Utc>,
        }
        let mut bound = key
            .filter(|_| !model_move)
            .and_then(|k| self.retained_binding(d, k, now))
            .map(|b| SelectionBinding {
                key: b.key.clone(),
                account: b.account.clone(),
                last_used: b.last_used,
            });
        if let (Some(anchor), Some(key)) = (&plan.anchor, key.filter(|_| !model_move)) {
            let b = bound.get_or_insert_with(|| SelectionBinding {
                key: key.to_owned(),
                account: anchor.clone(),
                last_used: now,
            });
            b.account = anchor.clone();
        }
        let bound_account = bound.as_ref().map(|b| b.account.clone());
        let mut reason = SelectionCode::New;
        if let Some(b) = bound {
            reason = SelectionCode::Forced;
            if let Some((c, eligible)) = entries.iter().find(|(c, _)| c.account.id == b.account) {
                if hold_allowed
                    && c.account.user_state == UserState::Active
                    && let Health::Cooling {
                        reason: HealthReason::Quota,
                        until,
                        scope,
                    } = &c.account.health
                    && let Some(quota) = &c.account.quota_cooling
                    && *until > now
                    && (scope == "account"
                        || scope
                            .strip_prefix("model:")
                            .is_some_and(|m| model == Some(m)))
                {
                    let reset = quota.reset_at;
                    if reset > now
                        && (reset - now).to_std().is_ok_and(|wait| {
                            wait <= Duration::from_secs(plan.settings.reset_wait_secs)
                        })
                    {
                        return (PreviewDecision::Hold { until: reset }, reason);
                    }
                    let skip_reason = if reset <= now {
                        "reset_elapsed"
                    } else if reset.signed_duration_since(now).to_std().is_err() {
                        "reset_before_now"
                    } else {
                        "reset_beyond_hold_bound"
                    };
                    tracing::debug!(
                        account = %b.account,
                        reason = skip_reason,
                        "reset hold skipped"
                    );
                }
                if *eligible && plan.settings.mode != crate::routing::Mode::Rebalance {
                    return (
                        PreviewDecision::Selected {
                            account: b.account.clone(),
                        },
                        if self.has_overload_mark(d, &b.key, now) {
                            SelectionCode::MarkWait
                        } else {
                            SelectionCode::Forced
                        },
                    );
                }
                if *eligible && d.active.contains_key(group(&b.key)) {
                    return (
                        PreviewDecision::Selected {
                            account: b.account.clone(),
                        },
                        if self.has_overload_mark(d, &b.key, now) {
                            SelectionCode::MarkWait
                        } else {
                            SelectionCode::Forced
                        },
                    );
                }
                let marked = self.has_overload_mark(d, &b.key, now);
                if marked && *eligible {
                    let known_cost = !d.unknown_cost.contains(group(&b.key))
                        && d.last_request_bytes
                            .get(group(&b.key))
                            .is_some_and(|n| *n > 0 && *n <= self.config.churn.max_move_bytes);
                    let room_elsewhere = known_cost
                        && entries.iter().any(|(candidate, eligible)| {
                            candidate.account.id != b.account
                                && *eligible
                                && d.overload_marks
                                    .get(group(&b.key))
                                    .is_some_and(|mark| mark.from == b.account)
                                && self.rebalance_target(
                                    d,
                                    &b.account,
                                    candidate,
                                    Some(group(&b.key)),
                                    now,
                                )
                                && self.policies.discretionary_allowed(
                                    policy_key.unwrap_or(&b.key),
                                    Some(&candidate.account.id),
                                    now,
                                )
                        });
                    // Any discretionary veto waits on the existing mark. In
                    // particular, fresh/uncapped targets must not erase a mark
                    // while group, revisit, pool or cost guards are active.
                    let guard_waiting = !known_cost
                        || !self.policies.discretionary_allowed(
                            policy_key.unwrap_or(&b.key),
                            None,
                            now,
                        )
                        || entries.iter().any(|(candidate, eligible)| {
                            *eligible
                                && candidate.account.id != b.account
                                && self.rebalance_target_waiting(
                                    d,
                                    &b.account,
                                    candidate,
                                    Some(group(&b.key)),
                                    now,
                                )
                                && (placement_capped(candidate, d, &self.config, now)
                                    || !self.policies.discretionary_allowed(
                                        policy_key.unwrap_or(&b.key),
                                        Some(&candidate.account.id),
                                        now,
                                    ))
                        });
                    if !room_elsewhere
                        && guard_waiting
                        && self.active_sessions_in(d, &b.account, now)
                            > self.spread_threshold_for(&b.account)
                    {
                        return (
                            PreviewDecision::Selected {
                                account: b.account.clone(),
                            },
                            SelectionCode::MarkWait,
                        );
                    }
                    if !room_elsewhere
                        || self.active_sessions_in(d, &b.account, now)
                            <= self.spread_threshold_for(&b.account)
                    {
                        // choose_with_hold clears this mark when it reserves
                        // the unchanged binding. Preview remains read-only.
                        return (
                            PreviewDecision::Selected {
                                account: b.account.clone(),
                            },
                            SelectionCode::Forced,
                        );
                    }
                    // Use the ordinary C7 ordering below, restricted to real
                    // eligible targets with room rather than candidate id order.
                    reason = SelectionCode::Overload;
                }
                if *eligible && !marked {
                    let g = group(&b.key);
                    let last = d.groups.get(g).map_or(b.last_used, |entry| entry.last_used);
                    let at_capacity = self.active_sessions_in(d, &b.account, now)
                        >= self.spread_threshold_for(&b.account);
                    let cold =
                        !protected(d, g) && idle(now, last) >= self.config.rebalance_idle_secs;
                    if !cold
                        || !self.policies.discretionary_allowed(
                            policy_key.unwrap_or(&b.key),
                            None,
                            now,
                        )
                        || d.unknown_cost.contains(group(&b.key))
                        || d.last_request_bytes.get(group(&b.key)).is_none_or(|bytes| {
                            *bytes == 0 || *bytes > self.config.churn.max_move_bytes
                        })
                        || (!at_capacity
                            && !c.account.drain
                            && metrics_without_placement_charge(c, d, &self.config, now).gate)
                    {
                        return (
                            PreviewDecision::Selected {
                                account: b.account.clone(),
                            },
                            reason,
                        );
                    }
                    reason = if marked {
                        SelectionCode::Overload
                    } else {
                        SelectionCode::ColdRebalance
                    };
                }
            }
        } else if let Some(p) = parent.and_then(|p| self.retained_binding(d, p, now)) {
            if entries
                .iter()
                .any(|(c, eligible)| c.account.id == p.account && *eligible)
            {
                return (
                    PreviewDecision::Selected {
                        account: p.account.clone(),
                    },
                    SelectionCode::Inherited,
                );
            }
        } else if key.is_none()
            && let Some(id) = d.sticky.get(provider.as_str())
            && entries
                .iter()
                .any(|(c, eligible)| &c.account.id == id && *eligible)
        {
            return (
                PreviewDecision::Selected {
                    account: id.clone(),
                },
                reason,
            );
        }
        let eligible: Vec<_> = entries
            .into_iter()
            .filter(|(c, eligible)| !c.account.drain && *eligible)
            .map(|(c, _)| (c, metrics(c, d, &self.config, now)))
            .collect();
        let eligible: Vec<_> = eligible
            .into_iter()
            .filter(|(c, _)| {
                if !matches!(
                    reason,
                    SelectionCode::Overload | SelectionCode::ColdRebalance
                ) {
                    return true;
                }
                bound_account.as_ref().is_some_and(|source| {
                    source != &c.account.id
                        && self.rebalance_target_base(d, source, c, key.map(group), now)
                }) && key.is_some_and(|key| {
                    self.policies.discretionary_allowed(
                        policy_key.unwrap_or(key),
                        Some(&c.account.id),
                        now,
                    )
                })
            })
            .collect();
        let has_gate = eligible.iter().any(|(_, m)| m.gate);
        let pool: Vec<_> = eligible
            .iter()
            .filter(|(_, m)| !has_gate || m.gate)
            .collect();
        let has_room = pool.iter().any(|(c, _)| {
            self.active_sessions_in(d, &c.account.id, now)
                + self.reserved_in(d, &c.account.id, key.map(group), now)
                < self.spread_threshold_for(&c.account.id)
        });
        let mut partition: Vec<_> = pool
            .into_iter()
            .filter(|(c, _)| {
                !has_room
                    || self.active_sessions_in(d, &c.account.id, now)
                        + self.reserved_in(d, &c.account.id, key.map(group), now)
                        < self.spread_threshold_for(&c.account.id)
            })
            .collect();
        if reason == SelectionCode::Overload {
            partition.retain(|(c, m)| {
                m.gate
                    && key
                        .and_then(|key| d.overload_marks.get(group(key)))
                        .is_some_and(|mark| {
                            mark.from != c.account.id
                                && self.rebalance_target_base(d, &mark.from, c, key.map(group), now)
                                && key.is_some_and(|key| {
                                    self.policies.discretionary_allowed(
                                        policy_key.unwrap_or(key),
                                        Some(&c.account.id),
                                        now,
                                    )
                                })
                        })
            });
        }
        if reason == SelectionCode::ColdRebalance {
            partition.retain(|(c, _)| {
                bound_account.as_ref().is_some_and(|source| {
                    source != &c.account.id
                        && self.rebalance_target_base(d, source, c, key.map(group), now)
                }) && key.is_some_and(|key| {
                    self.policies.discretionary_allowed(
                        policy_key.unwrap_or(key),
                        Some(&c.account.id),
                        now,
                    )
                })
            });
        }
        if matches!(
            reason,
            SelectionCode::Overload | SelectionCode::ColdRebalance
        ) {
            // A discretionary move cannot bypass a capped, non-pacing target
            // by reviving a pacing alternative. Resolve room, pacing, then cap.
            if partition
                .iter()
                .any(|(c, _)| !d.pacing.get(&c.account.id).copied().unwrap_or(false))
            {
                partition.retain(|(c, _)| !d.pacing.get(&c.account.id).copied().unwrap_or(false));
            }
            partition.retain(|(c, _)| !placement_capped(c, d, &self.config, now));
        }
        if partition.is_empty()
            && matches!(
                reason,
                SelectionCode::ColdRebalance | SelectionCode::Overload
            )
            && let Some(account) = bound_account
        {
            return (
                PreviewDecision::Selected { account },
                if reason == SelectionCode::Overload {
                    SelectionCode::MarkWait
                } else {
                    SelectionCode::Forced
                },
            );
        }
        let ratio = |c: &Candidate| {
            (self.active_sessions_in(d, &c.account.id, now)
                + self.reserved_in(d, &c.account.id, key.map(group), now)) as f64
                / self.spread_threshold_for(&c.account.id) as f64
        };
        let min_ratio = partition
            .iter()
            .map(|(c, _)| ratio(c))
            .min_by(f64::total_cmp);
        if !has_room && let Some(minimum) = min_ratio {
            partition.retain(|(c, _)| ratio(c) == minimum);
        }
        // Step 4: form the gate-passing set before any pacing/cap ordering.
        // A lower-count or uncapped failing account must never displace it.
        let mut gated: Vec<_> = partition
            .iter()
            .filter(|entry| entry.1.gate)
            .map(|entry| (entry.0, &entry.1))
            .collect();
        let chosen = if !gated.is_empty() {
            prefer_pacing_and_evidence(&mut gated, d, &self.config, now);
            launch::prefer(&mut gated, preference);
            // Approximate reset cohorts require one fixed anchor.
            let anchor = gated
                .iter()
                .filter(|(_, m)| !m.unknown)
                .filter_map(|(_, m)| m.weekly)
                .min();
            gated.sort_by(|(a, x), (b, y)| compare_gated(a, x, b, y, anchor));
            gated.first().map(|(candidate, _)| *candidate)
        } else {
            // Step 5: only when this set has no passing gate, apply its own
            // pacing/cap ordering and then choose the most headroom.
            let mut fallback: Vec<_> = partition.iter().map(|entry| (entry.0, &entry.1)).collect();
            prefer_pacing_and_evidence(&mut fallback, d, &self.config, now);
            launch::prefer(&mut fallback, preference);
            fallback
                .iter()
                .min_by(|(a, x), (b, y)| {
                    x.usage
                        .total_cmp(&y.usage)
                        .then_with(|| a.account.id.cmp(&b.account.id))
                })
                .map(|(candidate, _)| *candidate)
        };
        (
            chosen.map_or(PreviewDecision::None, |c| PreviewDecision::Selected {
                account: c.account.id.clone(),
            }),
            reason,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::tests::{candidate, fixture_dir};

    fn place(table: &Arc<BindingTable>, key: &str, candidates: &[Candidate]) {
        match table.choose(
            Provider::Claude,
            Some(key),
            None,
            None,
            candidates,
            None,
            100,
        ) {
            Decision::Selected { lease, .. } => lease.dispatched(),
            other => panic!("fixture placement failed: {other:?}"),
        }
    }

    #[test]
    fn n6t_cold_owner_binds_key_source_destination_model_and_selection_instant() {
        let root = fixture_dir();
        let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
        let key = "claude:sample-cold";
        let mut source = candidate("sample-source", 10.0, 30, 2);
        let target = candidate("sample-target", 5.0, 8, 1);
        place(&table, key, std::slice::from_ref(&source));
        let now = Utc::now();
        let _mutation = table.mutations.lock().unwrap();
        let mut d = table.data.lock().unwrap();
        for binding in d.bindings.values_mut() {
            binding.last_used = now - chrono::Duration::seconds(3601);
        }
        rebuild_indexes(&mut d);
        let candidates = [source.clone(), target.clone()];
        let (_, warm_reason) = table.select_ordered(
            &d,
            Provider::Claude,
            Some(key),
            None,
            None,
            &candidates,
            None,
            now,
            false,
            None,
        );
        assert!(
            warm_reason.evidence.is_none(),
            "a passing gate is not cold move evidence"
        );
        for window in source.quota.as_mut().unwrap()["windows"]
            .as_array_mut()
            .unwrap()
        {
            window["used_pct"] = serde_json::json!(91.0);
        }
        let candidates = [source, target];
        let before = snapshot(&d);
        let (choice, reason) = table.select_ordered(
            &d,
            Provider::Claude,
            Some(key),
            None,
            Some("sample-model"),
            &candidates,
            None,
            now,
            false,
            None,
        );
        assert_eq!(
            choice,
            PreviewDecision::Selected {
                account: "sample-target".into()
            }
        );
        assert_eq!(reason.code(), "cold_rebalance");
        let from = Some("sample-source");
        assert!(reason.valid_for(Some(key), from, "sample-target", Some("sample-model"), now));
        // A sibling belongs to the same affinity group and uses the same proof.
        assert!(reason.valid_for(
            Some("claude:sample-cold:agent:peer"),
            from,
            "sample-target",
            Some("sample-model"),
            now
        ));
        for (other_key, other_from, other_to, model, at) in [
            (
                Some("claude:other"),
                from,
                "sample-target",
                Some("sample-model"),
                now,
            ),
            (None, from, "sample-target", Some("sample-model"), now),
            (
                Some(key),
                Some("other-source"),
                "sample-target",
                Some("sample-model"),
                now,
            ),
            (Some(key), None, "sample-target", Some("sample-model"), now),
            (Some(key), from, "other-target", Some("sample-model"), now),
            (Some(key), from, "sample-target", Some("other-model"), now),
            (Some(key), from, "sample-target", None, now),
            (
                Some(key),
                from,
                "sample-target",
                Some("sample-model"),
                now + chrono::Duration::nanoseconds(1),
            ),
        ] {
            assert!(
                !reason.valid_for(other_key, other_from, other_to, model, at),
                "foreign discretionary context must not authorize a move"
            );
        }
        assert_eq!(
            serde_json::to_value(snapshot(&d)).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        // A breaker veto may keep the source; that needs no move capability.
        assert!(reason.valid_for(Some(key), from, "sample-source", None, now));
    }

    #[test]
    fn n6t_overload_evidence_requires_an_actual_mark_and_yields_to_an_active_group() {
        let root = fixture_dir();
        let table = BindingTable::new(
            root.path(),
            RoutingConfig {
                spread_threshold: 2,
                ..RoutingConfig::default()
            },
        )
        .unwrap();
        let candidates = [
            candidate("sample-source", 10.0, 8, 1),
            candidate("sample-target", 10.0, 30, 2),
        ];
        for n in 0..6 {
            place(&table, &format!("claude:sample-{n}"), &candidates[..1]);
        }
        let now = Utc::now();
        let mut marked = 0;
        for n in 0..3 {
            marked += table
                .note_client_rate(
                    Provider::Claude,
                    "sample-source",
                    Some(&format!("claude:sample-{n}")),
                    &candidates,
                    now,
                )
                .3;
        }
        assert!(marked > 0);
        let key = table
            .data
            .lock()
            .unwrap()
            .overload_marks
            .keys()
            .next()
            .unwrap()
            .clone();
        {
            let _mutation = table.mutations.lock().unwrap();
            let d = table.data.lock().unwrap();
            let (choice, reason) = table.select_ordered(
                &d,
                Provider::Claude,
                Some(&key),
                None,
                None,
                &candidates,
                None,
                now,
                false,
                None,
            );
            assert_eq!(
                choice,
                PreviewDecision::Selected {
                    account: "sample-target".into()
                }
            );
            assert_eq!(reason.code(), "overload");
            assert!(reason.valid_for(
                Some(&key),
                Some("sample-source"),
                "sample-target",
                None,
                now
            ));
        }
        let active = table.begin_request(Some(&key), "sample-source");
        let _mutation = table.mutations.lock().unwrap();
        let d = table.data.lock().unwrap();
        let (choice, reason) = table.select_ordered(
            &d,
            Provider::Claude,
            Some(&key),
            None,
            None,
            &candidates,
            None,
            now,
            false,
            None,
        );
        assert_eq!(
            choice,
            PreviewDecision::Selected {
                account: "sample-source".into()
            }
        );
        assert!(
            reason.evidence.is_none(),
            "active siblings cannot mint overload evidence"
        );
        drop(d);
        drop(_mutation);
        drop(active);
    }

    #[test]
    fn n6t_unaccepted_cold_row_uses_the_ranked_source_not_ws_protection() {
        let root = fixture_dir();
        let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
        let key = "claude:sample-unsent";
        let mut source = candidate("sample-source", 10.0, 30, 2);
        let target = candidate("sample-target", 5.0, 8, 1);
        // Reserve and cancel before send: the canonical row is real, but it
        // has not earned the accepted-session drain exemption.
        drop(table.choose(
            Provider::Claude,
            Some(key),
            None,
            None,
            std::slice::from_ref(&source),
            None,
            100,
        ));
        let now = Utc::now();
        {
            let _mutation = table.mutations.lock().unwrap();
            let mut d = table.data.lock().unwrap();
            assert_eq!(d.bindings[key].accepted, Some(false));
            for binding in d.bindings.values_mut() {
                binding.last_used = now - chrono::Duration::seconds(3601);
            }
            rebuild_indexes(&mut d);
            assert!(table.protected_source(&d, Some(key), None, now).is_none());
        }
        for window in source.quota.as_mut().unwrap()["windows"]
            .as_array_mut()
            .unwrap()
        {
            window["used_pct"] = serde_json::json!(91.0);
        }
        let candidates = [source, target];
        let selected = table.choose_with_hold_at_admitted_proof(
            Provider::Claude,
            Some(key),
            None,
            None,
            &candidates,
            &candidates,
            MoveObservations::untracked(),
            None,
            100,
            false,
            false,
            now,
            None,
            |_| Some(DispatchAdmission::ordinary()),
        );
        match selected {
            Decision::Selected {
                account,
                lease,
                moves,
            } => {
                assert_eq!(
                    account, "sample-target",
                    "unaccepted retained rows can still become cold"
                );
                assert_eq!(moves.len(), 1);
                assert_eq!(moves[0].reason, "cold_rebalance");
                lease.dispatched();
            }
            other => panic!("cold evidence rejected the actual ranked source: {other:?}"),
        }
    }
}
