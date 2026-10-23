//! D22 data admission shares the binding transaction with ordinary routing.
use super::launch::{DataLaunchError, LaunchIngress};
use super::*;

pub(super) struct ManagedHttp {
    pub info: crate::bindings::LaunchInfo,
    pub model: String,
}

pub(super) fn error(completion: &mut Completion, failure: DataLaunchError) -> Response {
    completion.outcome(failure.status().as_u16(), failure.code());
    native_error(
        failure.status(),
        Provider::Codex,
        failure.code(),
        failure.message(),
    )
}

impl RelayState {
    fn clear_recovered_launch_hint(&self, managed: &ManagedHttp) -> bool {
        self.reconnect_exclusions.lock().is_ok_and(|mut hints| {
            hints
                .remove(&(
                    managed.info.canonical_key.clone(),
                    managed.info.account.clone(),
                ))
                .is_some()
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn prepare_http_launch(
        &self,
        ingress: Option<&LaunchIngress>,
        session: &mut session::Session,
        headers: &HeaderMap,
        prefix: &[u8],
        auxiliary_search: bool,
        deadline: tokio::time::Instant,
    ) -> Result<Option<ManagedHttp>, DataLaunchError> {
        // C8 is already classified by the caller. Never derive identity from
        // a new body field or from the routing token itself.
        let Some(info) = self
            .resolve_data_launch(ingress, session.key.as_deref(), deadline)
            .await?
        else {
            return Ok(None);
        };
        session.key = Some(info.canonical_key.clone());
        if !info.managed {
            return Ok(None);
        }
        // Search has its own payload schema and no completion-model selector.
        // Its C8 header identity still uses this launch's authoritative binding.
        let requested = if auxiliary_search {
            info.current_model.clone().ok_or(DataLaunchError::Invalid)?
        } else {
            launch::request_model(headers, prefix).map_err(|_| DataLaunchError::Invalid)?
        };
        let model = self
            .admit_data_model(&info, Some(&requested), deadline)
            .await?
            .ok_or(DataLaunchError::Invalid)?;
        session.model = Some(model.clone());
        Ok(Some(ManagedHttp { info, model }))
    }

    /// Initial HTTP only. Does not enter or modify the shared G2 wait helper.
    /// The request's original data deadline is passed unchanged into inspection.
    pub(super) async fn initial_http_far_wait(
        &self,
        provider: Provider,
        session: &session::Session,
        model: Option<&str>,
        deadline: tokio::time::Instant,
        ingress: Option<&LaunchIngress>,
    ) -> Option<Response> {
        let key = session.key.as_deref()?;
        if key.is_empty()
            || key.len() > 1024
            || model.is_some_and(|value| value.len() > 256)
            || self.is_draining()
            || self.pins.get(provider, Some(key)).is_some()
        {
            return None;
        }
        let operation = crate::issue_operation::IssueOperation::new(deadline);
        let _caller = operation.caller_guard();
        let work_operation = operation.clone();
        let accounts = self.accounts.clone();
        let bindings = self.bindings.clone();
        let work_key = key.to_owned();
        let model = model.map(str::to_owned);
        let pin = ingress.and_then(|ingress| ingress.pin.clone());
        let task = tokio::task::spawn_blocking(move || {
            let fallback = accounts.current_http_initial_far_wait(
                &bindings,
                crate::bindings::HttpWaitRequest {
                    provider,
                    key: Some(&work_key),
                    model: model.as_deref(),
                    expected_source: None,
                    operation: &work_operation,
                },
            )?;
            let publication = fallback.as_ref().and_then(|fallback| {
                pin.as_deref().and_then(|pin| {
                    accounts
                        .register_initial_deferred_reset(
                            &bindings,
                            crate::bindings::HttpWaitRequest {
                                provider,
                                key: Some(&work_key),
                                model: model.as_deref(),
                                expected_source: None,
                                operation: &work_operation,
                            },
                            fallback,
                            pin,
                        )
                        .ok()
                        .flatten()
                        .map(|(id, scope)| {
                            native_reset::PendingPublication::new(accounts.clone(), scope, id)
                        })
                })
            });
            Ok::<_, ()>((fallback, publication))
        });
        let (fallback, publication) = tokio::select! {
            biased;
            _ = self.wait_for_drain_start() => return None,
            _ = tokio::time::sleep_until(deadline) => return None,
            result = task => result.ok()?.ok()?,
        };
        let fallback = fallback?;
        if self.is_draining()
            || !operation.can_reply()
            || self.pins.get(provider, Some(key)).is_some()
        {
            return None;
        }
        // Reject a cut/replaced proof or changed policy while native work was
        // returning. Response facts were assembled under all native gates.
        let response = fallback.into_response(&self.bindings.policies)?;
        if self.is_draining() || !operation.can_reply() {
            return None;
        }
        if let Some(publication) = publication {
            publication.keep();
        }
        Some(response)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn select_http_request(
        self: &Arc<Self>,
        provider: Provider,
        session: &session::Session,
        completion: &mut Completion,
        model: Option<&str>,
        tried: &BTreeSet<String>,
        managed: Option<&ManagedHttp>,
    ) -> Result<Option<Account>, ()> {
        self.select_launch_request_admitted(
            provider, session, completion, model, tried, managed, None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn select_launch_request_admitted(
        self: &Arc<Self>,
        provider: Provider,
        session: &session::Session,
        completion: &mut Completion,
        model: Option<&str>,
        tried: &BTreeSet<String>,
        managed: Option<&ManagedHttp>,
        admit: Option<websocket::capacity::Admission>,
    ) -> Result<Option<Account>, ()> {
        let Some(managed) = managed else {
            if admit.is_none() {
                return self
                    .select_request(provider, session, completion, model, tried)
                    .await;
            }
            return self
                .select_request_admitted(provider, session, completion, model, tried, admit)
                .await;
        };
        let fresh = self
            .resolve_data_launch(
                None,
                Some(&managed.info.canonical_key),
                completion
                    .precommit_deadline
                    .map(tokio::time::Instant::from_std)
                    .unwrap_or_else(|| {
                        tokio::time::Instant::now() + Duration::from_secs(self.request_timeout_secs)
                    }),
            )
            .await
            .map_err(|_| ())?
            .ok_or(())?;
        if !fresh.managed {
            return Err(());
        }
        let current = ManagedHttp {
            info: fresh,
            model: managed.model.clone(),
        };
        // Adoption can redirect the caller's provisional key during model I/O.
        // Candidate lookup, policy selection and the committed lease must all
        // use the same fresh canonical identity checked by admission below.
        let canonical_session = session::Session {
            key: Some(current.info.canonical_key.clone()),
            parent: session.parent.clone(),
            model: session.model.clone(),
        };
        let expected = current.info.account.clone();
        let key = current.info.canonical_key.clone();
        let deadline = completion.precommit_deadline;
        let owner = Arc::new(());
        let weak_owner = Arc::downgrade(&owner);
        let relay = Arc::downgrade(self);
        // The chooser holds mutations (but releases data) while calling this
        // guard and through publication. A later adoption must reject this
        // choice before reserving capacity or publishing policy/bindings.
        let admit = Arc::new(move |account: &str| {
            weak_owner.upgrade().is_some()
                && deadline.is_none_or(|at| Instant::now() < at)
                && relay.upgrade().is_some_and(|relay| {
                    !relay.is_draining()
                        && relay
                            .bindings
                            .launch_info(&key, chrono::Utc::now())
                            .is_ok_and(|info| {
                                info.managed
                                    && info.canonical_key == key
                                    && info.account == expected
                            })
                })
                && admit.as_ref().is_none_or(|admit| admit(account))
        });
        let selected = self
            .select_request_configured(
                provider,
                &canonical_session,
                completion,
                Some(&managed.model),
                tried,
                Some(admit),
                Some(&current),
            )
            .await;
        drop(owner);
        selected
    }

    pub(super) async fn launch_candidates(
        &self,
        managed: &ManagedHttp,
        mut candidates: Vec<crate::bindings::Candidate>,
        tried: &BTreeSet<String>,
        deadline: tokio::time::Instant,
    ) -> Result<(Vec<crate::bindings::Candidate>, Option<BTreeSet<String>>), ()> {
        // An otherwise usable binding cannot be moved for menu maintenance,
        // overload balancing, or a new preference on an existing launch.
        let bound_usable = !tried.contains(&managed.info.account)
            && self
                .dispatch_data_allowed(
                    Some(&managed.info.canonical_key),
                    &managed.info.account,
                    Some(&managed.model),
                )
                .await;
        if bound_usable {
            // A reconnect exclusion is only a retry hint. Recovery of the
            // authoritative binding outranks it; otherwise narrowing a list
            // already filtered by that hint would produce an empty set.
            if self.clear_recovered_launch_hint(managed) {
                candidates = self
                    .candidates(
                        Provider::Codex,
                        Some(&managed.info.canonical_key),
                        Some(&managed.model),
                        tried,
                    )
                    .await?;
            }
            candidates.retain(|c| c.account.id == managed.info.account);
            return Ok((candidates, None));
        }
        let mut compatible = BTreeSet::new();
        let mut coverage = BTreeSet::new();
        for candidate in &candidates {
            // Retain the original account for C7's near-reset hold decision.
            // Unknown destination entitlements never authorize a move.
            if candidate.account.id == managed.info.account {
                compatible.insert(candidate.account.id.clone());
                continue;
            }
            if !candidate.account.eligible(Some(&managed.model)) || candidate.account.drain {
                continue;
            }
            if self.is_draining() {
                return Err(());
            }
            let catalog = tokio::time::timeout_at(
                deadline,
                self.launch_catalog(&candidate.account.id, false, false),
            )
            .await
            .map_err(|_| ())?;
            let Ok(catalog) = catalog else { continue };
            if catalog.supported().contains(&managed.model) {
                compatible.insert(candidate.account.id.clone());
                if managed
                    .info
                    .advertised_models
                    .is_subset(catalog.supported())
                {
                    coverage.insert(candidate.account.id.clone());
                }
            }
        }
        // Catalog I/O can outlive a pause or a reset. Do not publish a forced
        // move from the health snapshot taken before those waits.
        candidates = self
            .candidates(
                Provider::Codex,
                Some(&managed.info.canonical_key),
                Some(&managed.model),
                tried,
            )
            .await?;
        if !tried.contains(&managed.info.account)
            && self
                .dispatch_data_allowed(
                    Some(&managed.info.canonical_key),
                    &managed.info.account,
                    Some(&managed.model),
                )
                .await
        {
            if self.clear_recovered_launch_hint(managed) {
                candidates = self
                    .candidates(
                        Provider::Codex,
                        Some(&managed.info.canonical_key),
                        Some(&managed.model),
                        tried,
                    )
                    .await?;
            }
            candidates.retain(|c| c.account.id == managed.info.account);
            return Ok((candidates, None));
        }
        candidates.retain(|c| compatible.contains(&c.account.id));
        Ok((candidates, Some(coverage)))
    }

    pub(super) async fn guard_http_launch(
        &self,
        managed: &ManagedHttp,
        account: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(), DataLaunchError> {
        let fresh = self
            .resolve_data_launch(None, Some(&managed.info.canonical_key), deadline)
            .await?
            .ok_or(DataLaunchError::Changed)?;
        if !fresh.managed || fresh.account != account {
            return Err(DataLaunchError::Changed);
        }
        // Recheck after token/pacing/catalog awaits; account guard protects the
        // model observation. Final runtime/policy/current-account dispatch stays
        // in relay_inner, immediately before its original send/deadline guard.
        self.admit_data_model(&fresh, Some(&managed.model), deadline)
            .await?;
        Ok(())
    }
}
