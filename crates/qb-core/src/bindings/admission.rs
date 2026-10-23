//! Dispatch checks and optional evidence from the WS capacity owner.
//! This is not a substitute for a semantic move reason or a breaker permit.

pub(crate) struct DispatchAdmission {
    capacity: Option<crate::relay::CapacityMoveEvidence>,
    failure: Option<crate::relay::RequestFailureEvidence>,
}

impl DispatchAdmission {
    pub(crate) fn ordinary() -> Self {
        Self {
            capacity: None,
            failure: None,
        }
    }

    pub(crate) fn capacity(proof: crate::relay::CapacityMoveEvidence) -> Self {
        Self {
            capacity: Some(proof),
            failure: None,
        }
    }

    pub(crate) fn with_failure(
        mut self,
        failure: Option<crate::relay::RequestFailureEvidence>,
    ) -> Self {
        self.failure = failure;
        self
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
