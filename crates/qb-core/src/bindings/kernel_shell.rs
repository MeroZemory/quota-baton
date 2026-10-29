//! Native owner snapshots for the pure decision kernel. No decision is made by
//! this projection and no captured value can authorize a binding mutation.
use super::*;

mod selection;
pub(super) use selection::{
    SnapshotRefusal as SelectionSnapshotRefusal, snapshot as selection_snapshot,
};

pub(crate) type Recorder =
    Arc<dyn Fn(&kernel::DecisionInputs, &kernel::PermitDecision) -> Option<u64> + Send + Sync>;
pub(crate) type CommitRecorder = Arc<dyn Fn(Option<u64>) -> Option<u64> + Send + Sync>;

impl BindingTable {
    pub(crate) fn attach_tape(
        &self,
        producer: Arc<crate::observability::ReplayRuntime>,
        initial_accounts: &[Account],
    ) -> Result<(), decision_tape::StartupWitnessFailure> {
        use decision_tape::StartupWitnessFailure;
        let witness = (|| -> Result<(), StartupWitnessFailure> {
            use crate::observability::replay_recorder::{Event, Fingerprint, Owner};
            use sha2::{Digest, Sha256};
            let image = self.decision_tape_initial_image().map_err(|error| {
                error
                    .downcast_ref::<StartupWitnessFailure>()
                    .copied()
                    .unwrap_or(StartupWitnessFailure::ImageSerialization)
            })?;
            let bytes = serde_json::to_vec(&(&image, initial_accounts))
                .map_err(|_| StartupWitnessFailure::AccountsSerialization)?;
            let digest = Fingerprint(Sha256::digest(bytes).into());
            if producer
                .record(
                    Owner::Bindings,
                    None,
                    Event::InitialWitness {
                        component: Owner::Bindings,
                        revision: image.revision,
                        cursor: 0,
                        rows: image.bindings_rows,
                        digest,
                    },
                )
                .is_none()
            {
                return Err(StartupWitnessFailure::Publication);
            }
            Ok(())
        })();
        let selections = producer.clone();
        *self
            .selection_recorder
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(
            move |entry, capture_id, invocation, inputs, actual| {
                selections.record_selection(entry, capture_id, invocation, inputs, actual)
            },
        ));
        let permits = producer.clone();
        self.set_permit_recorder(Arc::new(move |inputs, actual| {
            permits.record_permit(inputs, actual)
        }));
        let commits = producer.clone();
        self.set_permit_commit_recorder(Arc::new(move |ordinal| {
            commits.record_permit_commit(ordinal)
        }));
        self.set_decision_recorder(Arc::new(move |record| producer.record_decision(record)));
        witness
    }

    pub(crate) fn set_permit_recorder(&self, recorder: Recorder) {
        *self
            .permit_recorder
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(recorder);
    }

    pub(crate) fn set_permit_commit_recorder(&self, recorder: CommitRecorder) {
        *self
            .permit_commit_recorder
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(recorder);
    }

    pub(super) fn record_permit_commit(&self, ordinal: Option<u64>) {
        let recorder = self
            .permit_commit_recorder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(recorder) = recorder
            && recorder(ordinal).is_none()
        {
            self.decision_capture_losses.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn record_permit(
        &self,
        inputs: &kernel::DecisionInputs,
        actual: &kernel::PermitDecision,
    ) -> Option<u64> {
        let recorder = self
            .permit_recorder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let recorder = recorder?;
        let ordinal = recorder(inputs, actual);
        if ordinal.is_none() {
            self.decision_capture_losses.fetch_add(1, Ordering::Relaxed);
        }
        ordinal
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn snapshot(
    d: &Data,
    key: &str,
    from: Option<&str>,
    to: &str,
    source: Option<&Candidate>,
    candidates: &[Candidate],
    model: Option<&str>,
    now: DateTime<Utc>,
    permit: Option<kernel::PermitInputs>,
) -> kernel::DecisionInputs {
    let candidate = |c: &Candidate| kernel::CandidateInput {
        account: c.account.clone(),
        quota: c.quota.clone(),
        inflight: c.inflight,
    };
    let mut accounts = BTreeMap::new();
    if let Some(target) = candidates.iter().find(|c| c.account.id == to) {
        accounts.insert(to.to_owned(), candidate(target));
    }
    if let Some(from) = from {
        // Source evidence is explicitly supplied by the native caller. It may
        // differ from the tried/filtered destination collection; never fill a
        // missing source by finding an apparently matching destination row.
        accounts.remove(from);
        if let Some(source) = source {
            accounts.insert(from.to_owned(), candidate(source));
        }
    }
    let account_ids: BTreeSet<_> = accounts.values().map(|c| c.account.id.as_str()).collect();
    let groups: BTreeMap<_, _> = d
        .groups
        .get(group(key))
        .into_iter()
        .map(|g| {
            (
                group(key).to_owned(),
                kernel::AcceptanceGroup {
                    account: g.account.clone(),
                    keys: g.keys.clone(),
                },
            )
        })
        .collect();
    let bindings = groups
        .values()
        .flat_map(|g| &g.keys)
        .filter_map(|key| {
            d.bindings.get(key).map(|binding| {
                (
                    key.clone(),
                    kernel::AcceptanceBinding {
                        accepted: binding.accepted,
                    },
                )
            })
        })
        .collect();
    let traffic = d
        .traffic
        .iter()
        .filter(|(id, _)| account_ids.contains(id.as_str()))
        .map(|(id, at)| (id.clone(), *at))
        .collect();
    let pending = d
        .pending
        .iter()
        .filter(|(id, _)| account_ids.contains(id.as_str()))
        .map(|(id, count)| (id.clone(), *count))
        .collect();
    kernel::DecisionInputs {
        selection: None,
        now,
        model: model.map(str::to_owned),
        key: Some(key.to_owned()),
        from: from.map(str::to_owned),
        to: to.to_owned(),
        accounts,
        traffic,
        pending,
        groups,
        bindings,
        // The permit entry always supplies Some(key), so accepted_in does not
        // read its distinct unbound/sticky branch in this input domain.
        accepted_unbound: BTreeSet::new(),
        sticky: BTreeMap::new(),
        outside_failure: d
            .outside_failure
            .as_ref()
            .is_some_and(|failed| failed.load(Ordering::Acquire)),
        permit,
    }
}

/// Selection uses the same raw helper layer before its own full capture slice.
/// The unused fields here are never serialized as a permit-entry snapshot.
pub(super) fn candidate_inputs(
    d: &Data,
    candidate: &Candidate,
    key: Option<&str>,
    model: Option<&str>,
    now: DateTime<Utc>,
) -> kernel::DecisionInputs {
    let mut inputs = snapshot(
        d,
        key.unwrap_or(""),
        None,
        &candidate.account.id,
        None,
        std::slice::from_ref(candidate),
        model,
        now,
        None,
    );
    inputs.key = key.map(str::to_owned);
    if key.is_none() {
        inputs.accepted_unbound = d.accepted_unbound.clone();
        inputs.sticky = d.sticky.clone();
    }
    inputs
}

pub(super) fn acceptance_inputs(d: &Data, key: Option<&str>, id: &str) -> kernel::DecisionInputs {
    let mut inputs = snapshot(
        d,
        key.unwrap_or(""),
        None,
        id,
        None,
        &[],
        None,
        DateTime::<Utc>::MIN_UTC,
        None,
    );
    // accepted_in has no clock/account/quota read. This adapter does not become
    // a tape input: the permit entry above supplies its actual native now.
    inputs.key = key.map(str::to_owned);
    if key.is_none() {
        inputs.accepted_unbound = d.accepted_unbound.clone();
        inputs.sticky = d.sticky.clone();
    }
    inputs
}

pub(super) fn outcome(
    result: &std::result::Result<move_guard::Permit, move_guard::Denied>,
) -> kernel::PermitDecision {
    match result {
        Ok(permit) => permit.decision(),
        Err(denied) => kernel::PermitDecision::Denied(kernel::PermitDenied {
            reason: denied.reason,
            until: denied.until,
            newly_tripped: denied.newly_tripped,
        }),
    }
}

#[cfg(test)]
mod tests;
