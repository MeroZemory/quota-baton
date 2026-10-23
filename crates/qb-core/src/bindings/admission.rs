//! Dispatch checks and optional evidence from the WS capacity owner.
//! This is not a substitute for a semantic move reason or a breaker permit.

pub(crate) struct DispatchAdmission {
    capacity: Option<crate::relay::CapacityMoveEvidence>,
    failure: Option<crate::relay::RequestFailureEvidence>,
    pin: Option<crate::debug_pin::DebugPinEvidence>,
    store: Vec<crate::accounts::StoreUnavailableEvidence>,
}

impl DispatchAdmission {
    pub(crate) fn ordinary() -> Self {
        Self {
            capacity: None,
            failure: None,
            pin: None,
            store: Vec::new(),
        }
    }

    pub(crate) fn capacity(proof: crate::relay::CapacityMoveEvidence) -> Self {
        Self {
            capacity: Some(proof),
            failure: None,
            pin: None,
            store: Vec::new(),
        }
    }

    pub(crate) fn with_failure(
        mut self,
        failure: Option<crate::relay::RequestFailureEvidence>,
    ) -> Self {
        self.failure = failure;
        self
    }

    pub(crate) fn with_pin(mut self, pin: Option<crate::debug_pin::DebugPinEvidence>) -> Self {
        self.pin = pin;
        self
    }

    pub(crate) fn with_store_evidence(
        mut self,
        store: Vec<crate::accounts::StoreUnavailableEvidence>,
    ) -> Self {
        self.store = store;
        self
    }

    pub(super) fn valid_store_for(
        &self,
        from: Option<&str>,
        source: Option<&crate::accounts::Account>,
        to: &str,
    ) -> bool {
        from.is_none_or(|from| {
            from == to
                || self
                    .store
                    .iter()
                    .filter(|proof| proof.source() == from)
                    .all(|proof| proof.applies_to(from, source))
        })
    }

    pub(super) fn valid_pin_for(
        &self,
        provider: crate::accounts::Provider,
        key: Option<&str>,
        to: &str,
    ) -> bool {
        self.pin.as_ref().is_none_or(|proof| {
            // An ineligible pin can be bypassed by normal selection, as before.
            proof.target() != to || proof.applies_to(provider, key, to)
        })
    }

    pub(super) fn valid_failure_for(
        &self,
        key: Option<&str>,
        source: Option<&crate::accounts::Account>,
        to: &str,
        model: Option<&str>,
    ) -> bool {
        self.failure.as_ref().is_none_or(|proof| {
            source.is_some_and(|source| {
                source.id == to || proof.applies_to(key, source, model, std::time::Instant::now())
            })
        })
    }

    pub(super) fn valid_for(&self, from: Option<&str>, to: &str) -> bool {
        self.capacity
            .as_ref()
            .is_none_or(|proof| from.is_some_and(|from| proof.applies_to(from, to)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::Provider,
        bindings::{BindingTable, Decision, MoveObservations},
        debug_pin::DebugPins,
    };

    #[test]
    fn n6t_pin_owner_veto_precedes_policy_publication_and_move_charge() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/n6t-pin");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let table =
            BindingTable::new(root.path(), crate::config::RoutingConfig::default()).unwrap();
        let candidates: Vec<_> = ["sample-source", "sample-target", "sample-next"]
            .into_iter()
            .map(|id| crate::bindings::tests::candidate(id, 10.0, 24, 4))
            .collect();
        let key = "claude:sample-pin";
        match table.choose(
            Provider::Claude,
            Some(key),
            None,
            None,
            &candidates[..1],
            None,
            0,
        ) {
            Decision::Selected { lease, .. } => lease.dispatched(),
            other => panic!("initial selection: {other:?}"),
        }
        let pins = DebugPins::default();
        let before = table.export().unwrap();
        let revision = table.routing_plan(Some(key)).revision;
        for changed in [false, true] {
            pins.set(Provider::Claude, None, "sample-target".into());
            let proof = pins
                .selection(
                    Provider::Claude,
                    Some(if changed { key } else { "claude:foreign" }),
                )
                .unwrap();
            let result = table.choose_with_hold_at_admitted_proof(
                Provider::Claude,
                Some(key),
                None,
                None,
                &candidates,
                &candidates,
                MoveObservations::untracked(),
                Some("sample-target"),
                0,
                false,
                false,
                chrono::Utc::now(),
                None,
                |_| {
                    if changed {
                        pins.set(Provider::Claude, None, "sample-next".into());
                    }
                    Some(DispatchAdmission::ordinary().with_pin(Some(proof)))
                },
            );
            assert!(
                matches!(result, Decision::None),
                "foreign or revoked pin must precede policy publication"
            );
            assert_eq!(table.export().unwrap(), before);
            assert_eq!(table.routing_plan(Some(key)).revision, revision);
        }
        pins.set(Provider::Claude, None, "sample-target".into());
        let proof = pins.selection(Provider::Claude, Some(key)).unwrap();
        let result = table.choose_with_hold_at_admitted_proof(
            Provider::Claude,
            Some(key),
            None,
            None,
            &candidates,
            &candidates,
            MoveObservations::untracked(),
            Some("sample-target"),
            0,
            false,
            false,
            chrono::Utc::now(),
            None,
            |_| Some(DispatchAdmission::ordinary().with_pin(Some(proof))),
        );
        assert!(
            matches!(result, Decision::Selected { ref account, .. } if account == "sample-target"),
            "veto must not spend the move budget"
        );
    }
}
