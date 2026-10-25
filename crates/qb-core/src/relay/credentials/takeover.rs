//! CPA transaction entrypoint, selected at the credentials module boundary.
use super::*;

impl TokenGates {
    /// Owns the WHOLE admitted transaction after caller cancellation. No
    /// production fallback exists when the explicit adapter is not installed.
    pub(crate) async fn takeover(
        &self,
        store: Arc<AccountStore>,
        obs: Arc<Observability>,
        request: crate::accounts::takeover::TakeoverRequest,
        dependencies: crate::accounts::takeover::Dependencies,
    ) -> Result<serde_json::Value, crate::accounts::takeover::TakeoverError> {
        use crate::accounts::takeover::{self, Preparation, ReceiptState, TakeoverError};
        #[cfg(any(test, feature = "testkit"))]
        if !crate::accounts::takeover::synthetic_endpoint(dependencies.profile.endpoint()) {
            return Err(TakeoverError::Invalid);
        }
        let activity = self.lifecycle.admit().ok_or(TakeoverError::Unavailable)?;
        let mut this = self.clone();
        this.takeover_context = this
            .takeover_context
            .with_dependencies(dependencies.clone());
        crate::boxes::spawn(async move {
            let _activity = activity;
            // Serialize source snapshots as well as new identities. The account
            // gate below is still shared with every D17 token consumer.
            let _admission = if request.phase == takeover::Phase::Commit {
                Some(this.takeover_context.admission().lock().await)
            } else {
                None
            };
            // prepare is read-only for inspect. Profile GET never refreshes.
            let preparation =
                takeover::prepare(store.clone(), dependencies.clone(), request).await?;
            let journal = match preparation {
                Preparation::Inspect(value) => return Ok(value),
                Preparation::Commit(journal) => *journal,
            };
            if journal.result.is_some() {
                return Ok(takeover::result_value(&journal));
            }
            let gate = this.gate(&journal.account.id).await;
            let guard = gate.lock_owned().await;
            if guard.pending.is_some() || this.lifecycle.stopped() {
                return Err(TakeoverError::Unavailable);
            }
            let install_store = store.clone();
            let install_dependencies = dependencies.clone();
            // The child blocking task retains lifecycle activity if this
            // detached parent is ever aborted, as do existing D17 read tasks.
            let write_activity = this.lifecycle.track_write();
            let journal = crate::boxes::spawn_blocking(move || {
                let _activity = write_activity;
                let _guard = guard;
                takeover::install(&install_store, &install_dependencies, journal)
            })
            .await
            .map_err(|_| TakeoverError::Unavailable)??;
            let receipt = store
                .takeover_receipt(&journal.account.id)?
                .ok_or(TakeoverError::Invalid)?;
            if receipt.operation != journal.operation {
                return Err(TakeoverError::Conflict);
            }
            match receipt.state {
                ReceiptState::Ready => {
                    let _ = this
                        .refresh_inner(
                            store.clone(),
                            obs,
                            journal.account.id.clone(),
                            None,
                            "takeover",
                            RefreshContext {
                                manual_baseline: None,
                                takeover_operation: Some(journal.operation.clone()),
                            },
                        )
                        .await;
                }
                ReceiptState::Started => return Err(TakeoverError::Uncertain),
                ReceiptState::Installed => return Err(TakeoverError::Unavailable),
                ReceiptState::Finished { .. } | ReceiptState::Superseded => {}
            }
            // A durable receipt, never uncertainty=false, determines completion.
            let finalize_activity = this.lifecycle.track_write();
            crate::boxes::spawn_blocking(move || {
                let _activity = finalize_activity;
                takeover::finalize(&store, journal)
            })
            .await
            .map_err(|_| TakeoverError::Unavailable)?
        })
        .await
        .map_err(|_| TakeoverError::Unavailable)?
    }
}
