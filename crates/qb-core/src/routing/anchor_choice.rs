//! Saved policy anchors reconcile stale bindings without asserting source health.
use super::*;

/// A store-owned anchor reconciliation fact, not dispatch or breaker authority.
/// Borrowing the owner keeps its identity alive for the lifetime of the proof.
pub(crate) struct AnchorChoiceEvidence<'a> {
    owner: &'a PolicyStore,
    key: String,
    from: String,
    to: String,
    revision: u64,
    mode: Mode,
    at: DateTime<Utc>,
}

impl PolicyStore {
    pub(crate) fn anchor_choice<'a>(
        &'a self,
        key: &str,
        from: &str,
        to: &str,
        at: DateTime<Utc>,
    ) -> Option<AnchorChoiceEvidence<'a>> {
        if from == to {
            return None;
        }
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
        let plan = self.anchor_choice_plan(key, to)?;
        Some(AnchorChoiceEvidence {
            owner: self,
            key: key.to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
            revision: plan.revision,
            mode: plan.settings.mode,
            at,
        })
    }

    // Caller holds publication access so state and authority flags stay coherent.
    fn anchor_choice_plan(&self, key: &str, to: &str) -> Option<Plan> {
        if self.snapshot().frozen {
            return None;
        }
        let plan = self.plan_published(Some(key));
        if plan.settings.mode == Mode::Strict
            && (self.strict_dispatch_blocked() || self.persistence_failed.load(Ordering::Acquire))
        {
            return None;
        }
        (plan.anchor.as_deref() == Some(to)).then_some(plan)
    }
}

impl AnchorChoiceEvidence<'_> {
    pub(crate) fn applies_to(
        &self,
        owner: &PolicyStore,
        key: Option<&str>,
        from: &str,
        to: &str,
        at: DateTime<Utc>,
    ) -> bool {
        if !std::ptr::eq(self.owner, owner)
            || key != Some(self.key.as_str())
            || from != self.from
            || to != self.to
            || at != self.at
        {
            return false;
        }
        let _publication = owner.publication.read().unwrap_or_else(|e| e.into_inner());
        owner
            .anchor_choice_plan(&self.key, to)
            .is_some_and(|plan| plan.revision == self.revision && plan.settings.mode == self.mode)
    }
}

/// A saved anchor can differ from a stale live image. This proves only the
/// authoritative source; a separate owner must justify leaving that source.
pub(crate) struct AnchorRecoveryEvidence<'a> {
    anchor: AnchorChoiceEvidence<'a>,
    destination: String,
    model: Option<String>,
}
impl PolicyStore {
    pub(crate) fn anchor_recovery<'a>(
        &'a self,
        key: &str,
        from: &str,
        to: &str,
        model: Option<&str>,
        at: DateTime<Utc>,
    ) -> Option<AnchorRecoveryEvidence<'a>> {
        let _publication = self.publication.read().unwrap_or_else(|e| e.into_inner());
        let plan = self.plan_published(Some(key));
        let anchor = plan.anchor.as_deref()?;
        if from == anchor || to == anchor {
            return None;
        }
        self.anchor_choice_plan(key, anchor)?;
        Some(AnchorRecoveryEvidence {
            anchor: AnchorChoiceEvidence {
                owner: self,
                key: key.to_owned(),
                from: from.to_owned(),
                to: anchor.to_owned(),
                revision: plan.revision,
                mode: plan.settings.mode,
                at,
            },
            destination: to.to_owned(),
            model: model.map(str::to_owned),
        })
    }
}
impl AnchorRecoveryEvidence<'_> {
    pub(crate) fn source(&self) -> &str {
        &self.anchor.to
    }
    pub(crate) fn applies_to(
        &self,
        owner: &PolicyStore,
        key: Option<&str>,
        from: &str,
        to: &str,
        model: Option<&str>,
        at: DateTime<Utc>,
    ) -> bool {
        to == self.destination
            && model == self.model.as_deref()
            && self.anchor.applies_to(owner, key, from, self.source(), at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "codex:synthetic-anchor-choice";
    const FROM: &str = "target";
    const TO: &str = "source";

    // No disk, config, account health or binding fixtures participate in proof issuance.
    fn fixture(mode: Mode, anchor: Option<&str>) -> PolicyStore {
        let mut state = State::default();
        state.sessions.insert(
            session_id(KEY),
            SessionPolicy {
                project: None,
                baseline: Settings::default(),
                explicit: Some(Settings {
                    mode,
                    ..Settings::default()
                }),
                anchor: anchor.map(str::to_owned),
                urgency: None,
                movements: Vec::new(),
            },
        );
        PolicyStore {
            path: PathBuf::new(),
            defaults: Settings::default(),
            limits: ChurnLimits::default(),
            state: Mutex::new(Arc::new(state)),
            writer: Mutex::new(()),
            publication: RwLock::new(()),
            publication_hook: Mutex::new(None),
            file_seen: AtomicBool::new(false),
            recovery_blocked: AtomicBool::new(false),
            lifecycle_recovery: AtomicBool::new(false),
            persistence_failed: AtomicBool::new(false),
            recovery_intent: Mutex::new(State::default()),
            durable_generation: Mutex::new(None),
            warnings: Mutex::new(WarningState::default()),
            changed: tokio::sync::Notify::new(),
            deferred_fences: Mutex::new(Vec::new()),
        }
    }

    fn edit(store: &PolicyStore, change: impl FnOnce(&mut State)) {
        let _publication = store.publication.write().unwrap();
        change(Arc::make_mut(&mut store.state.lock().unwrap()));
    }

    #[test]
    fn actual_saved_anchor_can_reconcile_a_stale_live_binding() {
        let at = Utc::now();
        for mode in [Mode::Strict, Mode::Recover, Mode::Rebalance] {
            let store = fixture(mode, Some(TO));
            let proof = store.anchor_choice(KEY, FROM, TO, at).unwrap();
            assert!(proof.applies_to(&store, Some(KEY), FROM, TO, at));
            assert!(store.anchor_choice(KEY, TO, TO, at).is_none());
            assert!(store.anchor_choice(KEY, TO, FROM, at).is_none());
            assert!(store.anchor_choice(KEY, FROM, "unanchored", at).is_none());
        }
    }

    #[test]
    fn recovery_proof_binds_logical_source_and_exact_physical_context() {
        let at = Utc::now();
        let store = fixture(Mode::Recover, Some(TO));
        let foreign = fixture(Mode::Recover, Some(TO));
        let proof = store
            .anchor_recovery(KEY, FROM, "third", Some("sample-model"), at)
            .unwrap();
        assert_eq!(proof.source(), TO);
        assert!(proof.applies_to(&store, Some(KEY), FROM, "third", Some("sample-model"), at));
        for (owner, key, from, to, model, time) in [
            (&foreign, Some(KEY), FROM, "third", Some("sample-model"), at),
            (&store, None, FROM, "third", Some("sample-model"), at),
            (
                &store,
                Some(KEY),
                "other",
                "third",
                Some("sample-model"),
                at,
            ),
            (&store, Some(KEY), FROM, "other", Some("sample-model"), at),
            (&store, Some(KEY), FROM, "third", None, at),
            (
                &store,
                Some(KEY),
                FROM,
                "third",
                Some("sample-model"),
                at + chrono::Duration::nanoseconds(1),
            ),
        ] {
            assert!(!proof.applies_to(owner, key, from, to, model, time));
        }
        assert!(store.anchor_recovery(KEY, FROM, FROM, None, at).is_some());
        assert!(store.anchor_recovery(KEY, TO, "third", None, at).is_none());
        assert!(store.anchor_recovery(KEY, FROM, TO, None, at).is_none());
        edit(&store, |state| state.revision += 1);
        assert!(!proof.applies_to(&store, Some(KEY), FROM, "third", Some("sample-model"), at));
    }

    #[test]
    fn absent_anchors_and_unanchored_defaults_do_not_grant_anchor_choice() {
        let at = Utc::now();
        for mode in [Mode::Strict, Mode::Recover, Mode::Rebalance] {
            let store = fixture(mode, None);
            assert!(store.anchor_choice(KEY, FROM, TO, at).is_none());
            assert!(store.anchor_choice("codex:absent", FROM, TO, at).is_none());
        }
        // Global Recover defaults alone provide no anchor authority.
        let mut store = fixture(Mode::Recover, None);
        store.defaults.mode = Mode::Recover;
        edit(&store, |state| state.sessions.clear());
        assert!(store.anchor_choice(KEY, FROM, TO, at).is_none());
    }

    #[test]
    fn proof_requires_its_actual_store_and_exact_context() {
        let at = Utc::now();
        let store = fixture(Mode::Recover, Some(TO));
        let foreign = fixture(Mode::Recover, Some(TO));
        let proof = store.anchor_choice(KEY, FROM, TO, at).unwrap();
        assert!(!proof.applies_to(&foreign, Some(KEY), FROM, TO, at));
        assert!(!proof.applies_to(&store, None, FROM, TO, at));
        assert!(!proof.applies_to(&store, Some("codex:other"), FROM, TO, at));
        assert!(!proof.applies_to(&store, Some(KEY), "other", TO, at));
        assert!(!proof.applies_to(&store, Some(KEY), FROM, "other", at));
        assert!(!proof.applies_to(
            &store,
            Some(KEY),
            FROM,
            TO,
            at + chrono::Duration::nanoseconds(1),
        ));
        assert!(proof.applies_to(&store, Some(KEY), FROM, TO, at));
    }

    #[test]
    fn revision_anchor_and_mode_are_revalidated() {
        let at = Utc::now();
        for changed in ["revision", "anchor", "missing", "mode", "rebalance"] {
            let store = fixture(Mode::Recover, Some(TO));
            let proof = store.anchor_choice(KEY, FROM, TO, at).unwrap();
            edit(&store, |state| match changed {
                "revision" => state.revision += 1,
                "missing" => state.sessions.clear(),
                "anchor" => {
                    state.sessions.get_mut(&session_id(KEY)).unwrap().anchor = Some("other".into());
                }
                _ => {
                    state
                        .sessions
                        .get_mut(&session_id(KEY))
                        .unwrap()
                        .explicit
                        .as_mut()
                        .unwrap()
                        .mode = if changed == "mode" {
                        Mode::Strict
                    } else {
                        Mode::Rebalance
                    };
                }
            });
            assert!(
                !proof.applies_to(&store, Some(KEY), FROM, TO, at),
                "{changed}"
            );
        }
    }

    #[test]
    fn blocked_authority_prevents_issuance_and_invalidates_existing_proofs() {
        let at = Utc::now();
        for mode in [Mode::Strict, Mode::Recover, Mode::Rebalance] {
            for blocked in ["frozen", "recovery", "lifecycle", "failed"] {
                let store = fixture(mode, Some(TO));
                let proof = store.anchor_choice(KEY, FROM, TO, at).unwrap();
                {
                    let _publication = store.publication.write().unwrap();
                    match blocked {
                        "frozen" => {
                            Arc::make_mut(&mut store.state.lock().unwrap()).frozen = true;
                        }
                        "recovery" => store.recovery_blocked.store(true, Ordering::Release),
                        "lifecycle" => store.lifecycle_recovery.store(true, Ordering::Release),
                        _ => store.persistence_failed.store(true, Ordering::Release),
                    }
                }
                let refused = blocked == "frozen" || mode == Mode::Strict;
                assert_eq!(
                    store.anchor_choice(KEY, FROM, TO, at).is_none(),
                    refused,
                    "{mode:?}/{blocked}"
                );
                assert_eq!(
                    !proof.applies_to(&store, Some(KEY), FROM, TO, at),
                    refused,
                    "{mode:?}/{blocked}"
                );
            }
        }
    }
}
