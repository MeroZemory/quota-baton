//! Dispatch checks and optional evidence from the WS capacity owner.
//! This is not a substitute for a semantic move reason or a breaker permit.

pub(crate) struct DispatchAdmission {
    capacity: Option<crate::relay::CapacityMoveEvidence>,
}

impl DispatchAdmission {
    pub(crate) fn ordinary() -> Self {
        Self { capacity: None }
    }

    pub(crate) fn capacity(proof: crate::relay::CapacityMoveEvidence) -> Self {
        Self {
            capacity: Some(proof),
        }
    }

    pub(super) fn valid_for(&self, from: Option<&str>, to: &str) -> bool {
        self.capacity
            .as_ref()
            .is_none_or(|proof| from.is_some_and(|from| proof.applies_to(from, to)))
    }
}
