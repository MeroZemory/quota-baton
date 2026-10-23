//! Sole canonical account replacement transaction. Labels never grant authority.
use super::*;
use std::sync::MutexGuard;

pub(super) enum MoveReason<'a> {
    Health(crate::accounts::health::UnusableEvidence),
    PolicyAnchor(crate::routing::AnchorChoiceEvidence<'a>),
    AnchoredRecovery(
        crate::routing::AnchorRecoveryEvidence<'a>,
        Box<MoveReason<'a>>,
    ),
    RuntimeUnavailable(&'a crate::account_runtime::RuntimeUnavailableEvidence),
    Capacity(&'a crate::relay::CapacityMoveEvidence),
    Rejected(&'a crate::relay::RequestFailureEvidence),
    StoreUnavailable(&'a crate::accounts::StoreUnavailableEvidence),
    UserPin(&'a crate::debug_pin::DebugPinEvidence),
    ExplicitModelConsent(&'a LaunchPolicyMove),
    Discretionary(&'a selection::RebalanceEvidence),
}

#[derive(Clone, Copy)]
pub(super) struct RebindContext<'a> {
    pub provider: Provider,
    pub key: Option<&'a str>,
    pub guard_key: &'a str,
    pub from: Option<&'a str>,
    pub account: &'a str,
    pub parent: Option<&'a str>,
    pub model: Option<&'a str>,
    pub candidates: &'a [Candidate],
    pub sources: &'a [Candidate],
    pub pinned: Option<&'a str>,
    pub request_bytes: u64,
    pub hold_timed_out: bool,
    pub now: DateTime<Utc>,
    pub label: &'a str,
}

pub(super) struct Rebound<'a> {
    pub data: MutexGuard<'a, Data>,
    pub moves: Vec<MoveEvent>,
}

pub(super) enum RebindError {
    Evidence,
    Policy,
}

impl MoveReason<'_> {
    fn label(&self) -> &'static str {
        match self {
            Self::ExplicitModelConsent(_) => "model_choice",
            Self::AnchoredRecovery(_, reason) => reason.label(),
            Self::Discretionary(proof) => proof.label(),
            _ => "forced",
        }
    }

    // Called only after valid(): diagnostic detail never supplies move authority.
    fn diagnostic_label(
        &self,
        cx: &RebindContext<'_>,
        table: &BindingTable,
        d: &Data,
    ) -> &'static str {
        if let Self::AnchoredRecovery(proof, reason) = self {
            let logical = RebindContext {
                from: Some(proof.source()),
                ..*cx
            };
            return reason.diagnostic_label(&logical, table, d);
        }
        let label = self.label();
        if label != "forced" {
            return label;
        }
        let Some(source) = cx
            .sources
            .iter()
            .find(|c| Some(c.account.id.as_str()) == cx.from)
        else {
            return label;
        };
        let source = effective_account(source, d, &table.config, cx.model, cx.now);
        let plan = table.policies.plan(launch::policy_key(d, cx.key));
        if source.user_state == UserState::Active
            && let Health::Cooling {
                reason: HealthReason::Quota,
                until,
                scope,
            } = &source.health
            && *until > cx.now
            && (scope == "account"
                || scope
                    .strip_prefix("model:")
                    .is_some_and(|model| cx.model == Some(model)))
            && let Some(quota) = &source.quota_cooling
            && quota.window == "5h"
            && quota
                .reset_at
                .signed_duration_since(cx.now)
                .to_std()
                .is_ok_and(|wait| wait > Duration::from_secs(plan.settings.reset_wait_secs))
        {
            "five_hour_far_move"
        } else {
            label
        }
    }

    fn valid(
        &self,
        cx: &RebindContext<'_>,
        source: Option<&Account>,
        policies: &crate::routing::PolicyStore,
        policy_key: Option<&str>,
    ) -> bool {
        let Some(from) = cx.from else { return false };
        match self {
            Self::AnchoredRecovery(proof, reason) => {
                let logical = RebindContext {
                    from: Some(proof.source()),
                    ..*cx
                };
                proof.applies_to(policies, policy_key, from, cx.account, cx.model, cx.now)
                    && reason.valid(
                        &logical,
                        cx.sources
                            .iter()
                            .find(|c| c.account.id == proof.source())
                            .map(|c| &c.account),
                        policies,
                        policy_key,
                    )
            }
            Self::PolicyAnchor(proof) => {
                proof.applies_to(policies, policy_key, from, cx.account, cx.now)
            }
            Self::RuntimeUnavailable(proof) => proof.applies_to(from),
            Self::Health(proof) => proof.applies_to_context(from, cx.model, cx.now),
            Self::Capacity(proof) => proof.applies_to(from, cx.account),
            Self::Rejected(proof) => source.is_some_and(|source| {
                proof.applies_to(cx.key, source, cx.model, std::time::Instant::now())
            }),
            Self::StoreUnavailable(proof) => proof.applies_to(from, source),
            Self::UserPin(proof) => proof.applies_to(cx.provider, cx.key, cx.account),
            Self::ExplicitModelConsent(proof) => {
                proof.previous() == from
                    && proof.account() == cx.account
                    && proof.model() == cx.model.unwrap_or("")
                    && proof.at() == cx.now
            }
            Self::Discretionary(proof) => {
                proof.applies_to(cx.key, from, cx.account, cx.model, cx.now)
            }
        }
    }
}

impl BindingTable {
    // Ranking's saved anchor still owns a pruned live row. Actual live state
    // wins when present, so an anchor proof cannot replace a different source.
    pub(super) fn rebind_source(
        &self,
        d: &Data,
        provider: Provider,
        key: Option<&str>,
    ) -> Option<String> {
        key.and_then(|key| d.groups.get(group(key)).map(|g| g.account.clone()))
            .or_else(|| key.and_then(|_| self.policies.plan(launch::policy_key(d, key)).anchor))
            .or_else(|| {
                key.is_none()
                    .then(|| d.sticky.get(provider.as_str()).cloned())
                    .flatten()
            })
    }

    pub(super) fn rebind_budget_source(
        &self,
        d: &Data,
        provider: Provider,
        key: Option<&str>,
        to: &str,
    ) -> Option<String> {
        let physical = self.rebind_source(d, provider, key);
        if physical.as_deref() != Some(to) {
            return physical;
        }
        self.policies
            .plan(launch::policy_key(d, key))
            .anchor
            .or(physical)
    }

    pub(super) fn owner_move_reason<'a>(
        &'a self,
        cx: &RebindContext<'_>,
        admission: &'a DispatchAdmission,
        selection: Option<&'a selection::SelectionReason>,
        consent: Option<&'a LaunchPolicyMove>,
    ) -> Option<MoveReason<'a>> {
        let from = cx.from?;
        let source = cx.sources.iter().find(|c| c.account.id == from);
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(proof) = launch::policy_key(&d, cx.key).and_then(|key| {
            self.policies
                .anchor_recovery(key, from, cx.account, cx.model, cx.now)
        }) {
            let logical = RebindContext {
                from: Some(proof.source()),
                ..*cx
            };
            drop(d);
            let reason = self.owner_move_reason(&logical, admission, selection, consent)?;
            return Some(MoveReason::AnchoredRecovery(proof, Box::new(reason)));
        }
        if from == cx.account {
            return None;
        }
        if let Some(proof) = consent {
            return Some(MoveReason::ExplicitModelConsent(proof));
        }
        if let Some(reason) = admission.move_reason(
            cx.provider,
            cx.key,
            from,
            source.map(|c| &c.account),
            cx.account,
            cx.model,
        ) {
            return Some(reason);
        }
        if let Some(proof) = selection.and_then(selection::SelectionReason::evidence) {
            return Some(MoveReason::Discretionary(proof));
        }
        if let Some(proof) = launch::policy_key(&d, cx.key)
            .and_then(|key| self.policies.anchor_choice(key, from, cx.account, cx.now))
        {
            return Some(MoveReason::PolicyAnchor(proof));
        }
        source
            .and_then(|source| {
                crate::accounts::health::UnusableEvidence::observe(
                    &effective_account(source, &d, &self.config, cx.model, cx.now),
                    cx.model,
                    Self::accepted_in(&d, cx.key, from),
                    cx.now,
                )
            })
            .map(MoveReason::Health)
    }

    /// Caller retains mutations. Breaker preparation is a no-charge preflight;
    /// only this transaction can consume its permit and publish replacement.
    /// Authorization (including Strict consent CAS) runs AFTER proof validation.
    pub(super) fn rebind<'a>(
        &'a self,
        cx: RebindContext<'_>,
        reason: Option<MoveReason<'_>>,
        permit: Option<move_guard::Permit>,
        authorize: impl FnOnce(&str) -> Result<()>,
    ) -> std::result::Result<Rebound<'a>, RebindError> {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if self.rebind_source(&d, cx.provider, cx.key).as_deref() != cx.from {
            return Err(RebindError::Evidence);
        }
        let expected_guard = cx
            .key
            .map(group)
            .map(|key| launch::ownership_group(&d, key).to_owned())
            .unwrap_or_else(|| format!("unidentified:{}", cx.provider.as_str()));
        if cx.guard_key != expected_guard {
            return Err(RebindError::Evidence);
        }
        let budget_from = self.rebind_budget_source(&d, cx.provider, cx.key, cx.account);
        let moving = budget_from
            .as_deref()
            .is_some_and(|from| from != cx.account);
        let source = cx
            .from
            .and_then(|from| cx.sources.iter().find(|c| c.account.id == from))
            .map(|c| &c.account);
        if moving
            && (!permit.as_ref().is_some_and(|permit| {
                permit.applies_to(
                    cx.guard_key,
                    budget_from.as_deref().unwrap(),
                    cx.account,
                    cx.now,
                )
            }) || !reason.as_ref().is_some_and(|reason| {
                reason.valid(&cx, source, &self.policies, launch::policy_key(&d, cx.key))
            }))
        {
            return Err(RebindError::Evidence);
        }
        if !moving && permit.is_some() {
            return Err(RebindError::Evidence);
        }
        let label = if moving {
            reason.as_ref().unwrap().diagnostic_label(&cx, self, &d)
        } else {
            cx.label
        };
        drop(d);
        authorize(label).map_err(|_| RebindError::Policy)?;
        let committed = self.commit_move(
            permit,
            cx.account,
            cx.provider,
            cx.guard_key,
            cx.model,
            cx.now,
        );
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        d.defer_count_changes = true;
        let moves = self.publish_binding(
            &mut d,
            cx.provider,
            cx.key,
            cx.parent,
            cx.candidates,
            cx.pinned,
            cx.request_bytes,
            cx.hold_timed_out,
            cx.now,
            cx.account,
            label,
            committed.as_ref(),
        );
        Ok(Rebound { data: d, moves })
    }
    fn commit_move(
        &self,
        permit: Option<move_guard::Permit>,
        to: &str,
        provider: Provider,
        key: &str,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<move_guard::CommittedMove> {
        if let Some(permit) = permit {
            // Internal invariant: neither clear nor rename can interleave under
            // mutations. A stale permit never changes a binding.
            let committed = self
                .move_guard
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .commit(permit, to)
                .expect("move permit serialized by mutations");
            if let Some(obs) = self.move_observability()
                && let Err(error) = obs.committed_move(provider, key, to, model, now)
            {
                tracing::warn!(
                    ?error,
                    "move cost observation rejected; coverage incomplete"
                );
            }
            Some(committed)
        } else {
            None
        }
    }
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn commit_binding(
        &self,
        d: &mut Data,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        request_bytes: u64,
        hold_timed_out: bool,
        now: DateTime<Utc>,
        account: &str,
        reason: &str,
    ) -> Vec<MoveEvent> {
        let guard_key = key.map(group).unwrap_or("unidentified:test");
        let from = key
            .and_then(|k| d.groups.get(group(k)))
            .map(|g| g.account.as_str());
        let permit = self
            .prepare_move(
                d,
                guard_key,
                from,
                account,
                candidates,
                MoveObservations::untracked(),
                None,
                reason,
                now,
            )
            .expect("fixture move admitted");
        let committed = self.commit_move(permit, account, provider, guard_key, None, now);
        self.publish_binding(
            d,
            provider,
            key,
            parent,
            candidates,
            pinned,
            request_bytes,
            hold_timed_out,
            now,
            account,
            reason,
            committed.as_ref(),
        )
    }
    // Shared commit: launch placement deliberately stops before reserve().
    #[allow(clippy::too_many_arguments)]
    fn publish_binding(
        &self,
        d: &mut Data,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        request_bytes: u64,
        hold_timed_out: bool,
        now: DateTime<Utc>,
        account: &str,
        reason: &str,
        committed: Option<&move_guard::CommittedMove>,
    ) -> Vec<MoveEvent> {
        d.count_change_clock = Some(now);
        let mut moves = Vec::new();
        if let Some(k) = key {
            let g = group(k);
            let accepted = Self::accepted_in(d, Some(k), account);
            let selected_reason = reason;
            let reason = if hold_timed_out {
                "reset_hold_timeout"
            } else {
                reason
            };
            if let Some(previous) = d.groups.get(g).filter(|entry| entry.account != account) {
                let authority = committed.expect("canonical rebind requires committed authority");
                assert!(
                    authority.for_group(launch::ownership_group(d, g))
                        && authority.allows(&previous.account, account),
                    "rebind receipt belongs to another source/group"
                );
            }
            let previous = unindex_group(d, g);
            let from = previous.as_ref().map(|entry| entry.account.to_owned());
            let source_changed = from.as_deref() != Some(account);
            // A stale physical row can already equal the destination while the
            // saved policy anchor still changes. Account that recovery as a
            // placement using the validated committed receipt, never a label.
            let placement_from = if !source_changed {
                committed
                    .and_then(|receipt| receipt.source_for(account))
                    .map(str::to_owned)
                    .or_else(|| from.clone())
            } else {
                from.clone()
            };
            let last_used = previous.as_ref().map_or(now, |entry| entry.last_used);
            let mut keys = previous.map_or_else(BTreeSet::new, |entry| entry.keys);
            keys.insert(k.into());
            if let Some(p) = parent
                && !d.bindings.contains_key(p)
                && group(p) == g
            {
                keys.insert(p.into());
            }
            if placement_from.as_deref() != Some(account) {
                let candidate = candidates
                    .iter()
                    .find(|c| c.account.id == account)
                    .expect("selection came from candidates");
                let (evidence, placed_since_obs) = placement_evidence(candidate, d, now);
                let capped = evidence == PlacementEvidence::Weak
                    && placed_since_obs >= self.config.weak_evidence_new_sessions;
                // C5 recovery is a placement; step-1 debug pins and step-2
                // inheritance remain exempt even when the account changes.
                let is_placement = selected_reason != "inherited" && pinned != Some(account);
                if is_placement {
                    let seq = self.begin_usage_observation();
                    d.placements
                        .entry(account.to_owned())
                        .or_default()
                        .placed(now, seq);
                }
                d.epochs.remove(g);
                moves.push(MoveEvent {
                    key: g.to_owned(),
                    from: placement_from.clone(),
                    to: account.to_owned(),
                    reason: if placement_from.is_none() && selected_reason == "new" {
                        "new"
                    } else {
                        reason
                    }
                    .into(),
                    bytes: request_bytes,
                    keys: keys.len(),
                    debug_pin: pinned == Some(account),
                    evidence,
                    placement_charge_pct: if is_placement {
                        self.config.placement_allowance_pct
                    } else {
                        0.0
                    },
                    capped,
                    placed_since_obs,
                    is_placement,
                });
                if placement_from.is_some() {
                    d.after_move.insert(g.into());
                }
            }
            for k in &keys {
                let previous = d.bindings.get(k).cloned();
                if let Some(previous) = &previous {
                    d.snapshot_weight = d
                        .snapshot_weight
                        .saturating_sub(binding_bound(&previous.key, &previous.account));
                }
                d.snapshot_weight = d.snapshot_weight.saturating_add(binding_bound(k, account));
                let mut next = if let Some(mut binding) = previous.clone() {
                    if binding.account != account {
                        binding.rebind(
                            account,
                            committed.expect("existing binding requires authority"),
                        );
                    }
                    binding
                } else {
                    Binding::new(
                        k.clone(),
                        account.to_owned(),
                        last_used,
                        None,
                        None,
                        None,
                        None,
                        false,
                    )
                };
                next.last_used = last_used;
                next.accepted = Some(accepted);
                next.first_seen_at = previous.as_ref().map_or(Some(now), |b| b.first_seen_at);
                next.bound_since = if from.as_deref() != Some(account) {
                    Some(now)
                } else {
                    previous.as_ref().and_then(|b| b.bound_since)
                };
                next.client_kind = previous.as_ref().and_then(|b| b.client_kind);
                next.observed_here = previous.as_ref().is_none_or(|b| b.observed_here);
                d.bindings.insert(k.clone(), next);
            }
            index_group(
                d,
                g.to_owned(),
                Group {
                    keys,
                    account: account.to_owned(),
                    last_used,
                },
            );
            if k != g && !d.last_request_bytes.contains_key(g) {
                d.unknown_cost.insert(g.to_owned());
            }
            if request_bytes > 0 && !d.unknown_cost.contains(g) {
                let known = d.last_request_bytes.entry(g.to_owned()).or_default();
                *known = (*known).max(request_bytes);
            }
            if !matches!(selected_reason, "mark_wait" | "guard_keep") {
                d.overload_marks.remove(g);
            }
            d.changed();
            // Publish every member and the group index before waking readers.
            if source_changed {
                self.policies.changed.notify_waiters();
            }
        } else if let Some(old) = d
            .sticky
            .insert(provider.as_str().into(), account.to_owned())
            && old != account
        {
            d.accepted_unbound.remove(&old);
            d.epochs.remove(&format!("unbound:{old}"));
        }
        moves
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{candidate, fixture_dir};
    use super::*;

    #[test]
    fn n6t_recover_anchor_recovers_from_stale_live_image_with_source_evidence() {
        for mode in [
            crate::routing::Mode::Recover,
            crate::routing::Mode::Rebalance,
        ] {
            for same_destination in [false, true] {
                let root = fixture_dir();
                let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
                let key = "claude:sample-stale-anchor";
                let mut anchor = candidate("sample-anchor", 10., 24, 4);
                let stale = candidate("sample-stale", 10., 24, 4);
                let target = candidate("sample-target", 10., 24, 4);
                match table.choose(
                    Provider::Claude,
                    Some(key),
                    None,
                    None,
                    std::slice::from_ref(&stale),
                    None,
                    0,
                ) {
                    Decision::Selected { lease, .. } => lease.dispatched(),
                    other => panic!("initial: {other:?}"),
                }
                table
                    .policies
                    .set_session(
                        &crate::routing::session_id(key),
                        Some(crate::routing::Settings {
                            mode,
                            ..Default::default()
                        }),
                        None,
                        Some("sample-anchor"),
                    )
                    .unwrap();
                let destination = if same_destination { &stale } else { &target };
                let before = table.export().unwrap();
                let choose = |source: Candidate| {
                    table.choose_with_hold_at_admitted_proof(
                        Provider::Claude,
                        Some(key),
                        None,
                        None,
                        std::slice::from_ref(destination),
                        &[source, stale.clone(), target.clone()],
                        MoveObservations::untracked(),
                        None,
                        0,
                        false,
                        false,
                        Utc::now(),
                        None,
                        |_| Some(DispatchAdmission::ordinary()),
                    )
                };
                assert!(
                    matches!(choose(anchor.clone()), Decision::None),
                    "filtered healthy anchor cannot justify a logical move"
                );
                assert_eq!(table.export().unwrap(), before);
                let reset = Utc::now() + chrono::Duration::hours(2);
                anchor.account.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: reset,
                    scope: "account".into(),
                };
                anchor.account.quota_cooling = Some(crate::accounts::QuotaCooling {
                    window: "5h".into(),
                    reset_at: reset,
                    started_at: Utc::now(),
                });
                match choose(anchor) {
                    Decision::Selected { account, moves, .. } => {
                        assert_eq!(account, destination.account.id);
                        assert_eq!(
                            moves.len(),
                            1,
                            "logical recovery must emit a placement event"
                        );
                        assert_eq!(
                            moves[0].from.as_deref(),
                            Some(if same_destination {
                                "sample-anchor"
                            } else {
                                "sample-stale"
                            })
                        );
                        assert!(moves[0].is_placement);
                        assert_eq!(moves[0].reason, "five_hour_far_move");
                    }
                    other => panic!(
                        "unusable logical anchor must recover despite stale live image: {other:?}"
                    ),
                }
                assert_eq!(
                    table.data.lock().unwrap().placements[&destination.account.id].total,
                    if same_destination { 2 } else { 1 },
                    "logical recovery must pay the D20 placement charge"
                );
                assert_eq!(
                    table.routing_plan(Some(key)).anchor.as_deref(),
                    Some(destination.account.id.as_str())
                );
                assert_eq!(
                    table
                        .move_guard
                        .lock()
                        .unwrap()
                        .recent_moves(Utc::now())
                        .len(),
                    1,
                    "logical recovery must charge even if the physical row already matches"
                );
            }
        }
    }

    #[test]
    fn n6t_recover_anchor_restore_survives_strict_recovery_fence() {
        for mode in [
            crate::routing::Mode::Recover,
            crate::routing::Mode::Strict,
            crate::routing::Mode::Rebalance,
        ] {
            let root = fixture_dir();
            let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
            let key = "claude:sample-recovery-fence";
            let stale = candidate("sample-stale", 10., 24, 4);
            let anchor = candidate("sample-anchor", 10., 24, 4);
            drop(table.choose(
                Provider::Claude,
                Some(key),
                None,
                None,
                std::slice::from_ref(&stale),
                None,
                0,
            ));
            table
                .policies
                .set_session(
                    &crate::routing::session_id(key),
                    Some(crate::routing::Settings {
                        mode,
                        ..Default::default()
                    }),
                    None,
                    Some("sample-anchor"),
                )
                .unwrap();
            table.policies.begin_recovery();
            let result = table.choose_with_hold_at_admitted_proof(
                Provider::Claude,
                Some(key),
                None,
                None,
                std::slice::from_ref(&anchor),
                &[stale, anchor.clone()],
                MoveObservations::untracked(),
                None,
                0,
                false,
                false,
                Utc::now(),
                None,
                |_| Some(DispatchAdmission::ordinary()),
            );
            assert_eq!(
                matches!(result, Decision::Selected { .. }),
                mode != crate::routing::Mode::Strict,
                "strict-only recovery fence must not disable recover anchor reconciliation"
            );
        }
    }

    #[test]
    fn n6t_rebind_refuses_filtered_healthy_source_before_policy_or_charge() {
        let root = fixture_dir();
        let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
        let source = candidate("sample-source", 10., 24, 4);
        let target = candidate("sample-target", 10., 24, 4);
        let key = "claude:sample-rebind";
        match table.choose(
            Provider::Claude,
            Some(key),
            None,
            None,
            std::slice::from_ref(&source),
            None,
            0,
        ) {
            Decision::Selected { lease, .. } => lease.dispatched(),
            other => panic!("initial selection: {other:?}"),
        }
        let before = table.export().unwrap();
        let revision = table.routing_plan(Some(key)).revision;
        for pin in [None, Some("sample-target")] {
            let decision = table.choose_with_hold_at_admitted_proof(
                Provider::Claude,
                Some(key),
                None,
                None,
                std::slice::from_ref(&target),
                &[source.clone(), target.clone()],
                MoveObservations::untracked(),
                pin,
                0,
                false,
                false,
                Utc::now(),
                None,
                |_| Some(DispatchAdmission::ordinary()),
            );
            assert!(
                matches!(decision, Decision::None),
                "a label, pin string or filtered source is not move authority"
            );
            assert_eq!(table.export().unwrap(), before);
            assert_eq!(table.routing_plan(Some(key)).revision, revision);
            assert!(
                table
                    .move_guard
                    .lock()
                    .unwrap()
                    .recent_moves(Utc::now())
                    .is_empty()
            );
        }
        let mut blocked = source;
        blocked.account.health = Health::NeedsRelogin {
            reason: "synthetic revoked".into(),
            since: Utc::now(),
        };
        let decision = table.choose_with_hold_at_admitted_proof(
            Provider::Claude,
            Some(key),
            None,
            None,
            std::slice::from_ref(&target),
            &[blocked, target.clone()],
            MoveObservations::untracked(),
            None,
            0,
            false,
            false,
            Utc::now(),
            None,
            |_| Some(DispatchAdmission::ordinary()),
        );
        assert!(
            matches!(decision, Decision::Selected { account, .. } if account == target.account.id),
            "actual health proof must still recover"
        );
    }

    #[test]
    fn n6t_rebind_charges_saved_anchor_after_live_binding_pruned() {
        let root = fixture_dir();
        let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
        let mut source = candidate("sample-source", 10., 24, 4);
        let target = candidate("sample-target", 10., 24, 4);
        let key = "claude:sample-pruned";
        match table.choose(
            Provider::Claude,
            Some(key),
            None,
            None,
            std::slice::from_ref(&source),
            None,
            0,
        ) {
            Decision::Selected { lease, .. } => lease.dispatched(),
            other => panic!("initial selection: {other:?}"),
        }
        table
            .policies
            .set_session(
                &crate::routing::session_id(key),
                Some(crate::routing::Settings {
                    mode: crate::routing::Mode::Recover,
                    ..Default::default()
                }),
                None,
                Some("sample-source"),
            )
            .unwrap();
        assert_eq!(
            table.routing_plan(Some(key)).anchor.as_deref(),
            Some("sample-source")
        );
        {
            let mut d = table.data.lock().unwrap();
            let entry = unindex_group(&mut d, key).unwrap();
            for key in entry.keys {
                d.bindings.remove(&key);
            }
            rebuild_indexes(&mut d);
        }
        source.account.health = Health::NeedsRelogin {
            reason: "synthetic revoked".into(),
            since: Utc::now(),
        };
        let decision = table.choose_with_hold_at_admitted_proof(
            Provider::Claude,
            Some(key),
            None,
            None,
            std::slice::from_ref(&target),
            &[source, target.clone()],
            MoveObservations::untracked(),
            None,
            0,
            false,
            false,
            Utc::now(),
            None,
            |_| Some(DispatchAdmission::ordinary()),
        );
        assert!(
            matches!(decision, Decision::Selected { account, .. } if account == target.account.id)
        );
        assert_eq!(
            table
                .move_guard
                .lock()
                .unwrap()
                .recent_moves(Utc::now())
                .len(),
            1,
            "saved-anchor recovery must spend a move even without a live row"
        );
    }

    #[test]
    fn n6t_foreign_prepared_permit_refuses_before_policy_authorization() {
        let root = fixture_dir();
        let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
        let mut source = candidate("sample-source", 10., 24, 4);
        let target = candidate("sample-target", 10., 24, 4);
        let key = "claude:sample-permit";
        drop(table.choose(
            Provider::Claude,
            Some(key),
            None,
            None,
            std::slice::from_ref(&source),
            None,
            0,
        ));
        let now = Utc::now();
        source.account.health = Health::NeedsRelogin {
            reason: "synthetic revoked".into(),
            since: now,
        };
        let sources = [source, target];
        let before = table.export().unwrap();
        let _mutation = table.mutations.lock().unwrap();
        let d = table.data.lock().unwrap();
        let permit = table
            .prepare_move_with_source(
                &d,
                "claude:foreign",
                Some("sample-source"),
                "sample-target",
                Some(&sources[0]),
                &sources,
                MoveObservations::untracked(),
                None,
                "forced",
                now,
            )
            .unwrap();
        drop(d);
        let context = RebindContext {
            provider: Provider::Claude,
            key: Some(key),
            guard_key: key,
            from: Some("sample-source"),
            account: "sample-target",
            parent: None,
            model: None,
            candidates: &sources,
            sources: &sources,
            pinned: None,
            request_bytes: 0,
            hold_timed_out: false,
            now,
            label: "forced",
        };
        let admission = DispatchAdmission::ordinary();
        let authority = table.owner_move_reason(&context, &admission, None, None);
        let called = std::cell::Cell::new(false);
        let result = table.rebind(context, authority, permit, |_| {
            called.set(true);
            bail!("synthetic policy veto");
        });
        assert!(
            !called.get(),
            "foreign permit must precede policy authorization"
        );
        assert!(matches!(result, Err(RebindError::Evidence)));
        drop(result);
        assert_eq!(table.export().unwrap(), before);
        assert!(
            table
                .move_guard
                .lock()
                .unwrap()
                .recent_moves(now)
                .is_empty()
        );
    }

    #[test]
    fn n6t_move_label_is_derived_from_owner_reason_before_policy() {
        for (caller_label, reset_after_hold_secs) in [
            ("cold_rebalance", None),
            ("model_choice", None),
            ("five_hour_far_move", None),
            ("five_hour_far_move", Some(0)),
            ("arbitrary", Some(1)),
        ] {
            let root = fixture_dir();
            let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
            let mut source = candidate("sample-source", 10., 24, 4);
            let target = candidate("sample-target", 10., 24, 4);
            let key = "claude:sample-label";
            drop(table.choose(
                Provider::Claude,
                Some(key),
                None,
                None,
                std::slice::from_ref(&source),
                None,
                0,
            ));
            let now = Utc::now();
            let expected = if let Some(extra) = reset_after_hold_secs {
                let hold = table.routing_plan(Some(key)).settings.reset_wait_secs;
                let reset = now + chrono::Duration::seconds(hold as i64 + extra);
                source.account.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: reset,
                    scope: "account".into(),
                };
                source.account.quota_cooling = Some(crate::accounts::QuotaCooling {
                    window: "5h".into(),
                    reset_at: reset,
                    started_at: now,
                });
                if extra > 0 {
                    "five_hour_far_move"
                } else {
                    "forced"
                }
            } else {
                source.account.user_state = UserState::Paused;
                "forced"
            };
            let sources = [source, target];
            let _mutation = table.mutations.lock().unwrap();
            let d = table.data.lock().unwrap();
            let permit = table
                .prepare_move_with_source(
                    &d,
                    key,
                    Some("sample-source"),
                    "sample-target",
                    Some(&sources[0]),
                    &sources,
                    MoveObservations::untracked(),
                    None,
                    caller_label,
                    now,
                )
                .unwrap();
            drop(d);
            let context = RebindContext {
                provider: Provider::Claude,
                key: Some(key),
                guard_key: key,
                from: Some("sample-source"),
                account: "sample-target",
                parent: None,
                model: None,
                candidates: &sources,
                sources: &sources,
                pinned: None,
                request_bytes: 0,
                hold_timed_out: false,
                now,
                label: caller_label,
            };
            let admission = DispatchAdmission::ordinary();
            let authority = table.owner_move_reason(&context, &admission, None, None);
            let rebound = table
                .rebind(context, authority, permit, |label| {
                    assert_eq!(
                        label, expected,
                        "owner reason, not caller label, must classify policy dispatch"
                    );
                    table.policies.dispatch(
                        Some(key),
                        "sample-target",
                        Some("sample-source"),
                        label,
                        now,
                    )
                })
                .unwrap_or_else(|_| panic!("valid owner move refused"));
            assert_eq!(rebound.moves[0].reason, expected);
            assert_eq!(rebound.data.groups[key].account, "sample-target");
        }
    }

    #[test]
    fn n6t_health_reason_cannot_cross_model_context() {
        let now = Utc::now();
        let mut source = candidate("sample-source", 10., 24, 4);
        source.account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: now + chrono::Duration::minutes(10),
            scope: "model:sample-model".into(),
        };
        let proof = crate::accounts::health::UnusableEvidence::observe(
            &source.account,
            Some("sample-model"),
            true,
            now,
        )
        .unwrap();
        assert!(proof.applies_to_context("sample-source", Some("sample-model"), now));
        assert!(!proof.applies_to_context("sample-source", Some("other-model"), now));
        assert!(!proof.applies_to_context("sample-source", None, now));
    }
}
