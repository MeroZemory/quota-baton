pub(super) mod capacity;
mod pump;

use super::*;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        handshake::{client::generate_key, derive_accept_key},
        protocol::{Role, WebSocketConfig},
    },
};
// Remember the retry token across sockets without retaining credential bytes or
// keeping a RelayState alive. Each live relay has at most one entry per account.
const RETRY_MARKER_TTL: Duration = Duration::from_secs(120);
struct RetriedToken {
    relay: std::sync::Weak<RelayState>,
    account: String,
    digest: [u8; 32],
    expires_at: std::time::Instant,
}
impl RetriedToken {
    fn matches(&self, digest: &[u8; 32], now: std::time::Instant) -> bool {
        self.expires_at > now && self.digest == *digest
    }
}
static RETRIED_TOKENS: std::sync::Mutex<Vec<RetriedToken>> = std::sync::Mutex::new(Vec::new());

fn token_retried(relay: &Arc<RelayState>, account: &str, token: &str, mark: bool) -> bool {
    use sha2::{Digest, Sha256};
    let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let mut entries = RETRIED_TOKENS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let now = std::time::Instant::now();
    entries.retain(|entry| entry.relay.strong_count() > 0 && entry.expires_at > now);
    let owner = Arc::downgrade(relay);
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.relay.ptr_eq(&owner) && entry.account == account)
    {
        let matched = entry.matches(&digest, now);
        if mark {
            entry.digest = digest;
            entry.expires_at = now + RETRY_MARKER_TTL;
        }
        return matched;
    }
    if mark {
        entries.push(RetriedToken {
            relay: owner,
            account: account.into(),
            digest,
            expires_at: now + RETRY_MARKER_TTL,
        });
    }
    false
}

fn clear_retried_token(relay: &Arc<RelayState>, account: &str, token: &str) {
    use sha2::{Digest, Sha256};
    let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let owner = Arc::downgrade(relay);
    let now = std::time::Instant::now();
    RETRIED_TOKENS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .retain(|entry| {
            entry.relay.strong_count() > 0
                && entry.expires_at > now
                && !(entry.relay.ptr_eq(&owner)
                    && entry.account == account
                    && entry.digest == digest)
        });
}

#[cfg(feature = "testkit")]
impl RelayState {
    pub fn ws_capacity_waits_for_test(&self) -> usize {
        self.ws_capacity.waits_for_test()
    }

    #[cfg(feature = "testkit")]
    pub fn pause_next_ws_own_wait(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        self.ws_capacity.pause_own_wait()
    }

    pub fn ws_reserved_for_test(&self, account: &str) -> usize {
        self.ws_capacity.reserved_for_test(account)
    }

    /// Pause the blocking selector under the mutation gate before admission.
    pub fn pause_next_ws_admission(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        self.ws_capacity.pause_admission()
    }

    /// Pause one synthetic WS after candidate collection, before guarded selection.
    pub fn pause_next_ws_selection(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        self.ws_capacity.pause_selection()
    }

    /// Pause one managed create after pacing, before its final dispatch gate.
    pub fn pause_next_ws_create_dispatch(
        self: &Arc<Self>,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        pump::pause_create_dispatch(self)
    }
}

pub(super) type UpstreamSocket = WebSocketStream<reqwest::Upgraded>;
pub(super) type ClientSocket = WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>;

async fn redial_same_account(
    context: &pump::Context,
    model: Option<&str>,
) -> Result<UpstreamSocket, &'static str> {
    let failure = "upstream_write_error";
    let relay = context.relay.as_ref().ok_or(failure)?;
    let (account, token) = context.credentials.as_ref().ok_or(failure)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    context
        .guard_launch_model(model, deadline)
        .await
        .map_err(|error| error.code())?;
    if relay.is_draining()
        || !relay
            .dispatch_data_allowed(context.session.key.as_deref(), &account.id, model)
            .await
    {
        return Err(failure);
    }
    let mut forwarded = headers::upstream(&context.headers, account, token).map_err(|_| failure)?;
    let key = generate_key();
    forwarded.insert("connection", HeaderValue::from_static("Upgrade"));
    forwarded.insert("upgrade", HeaderValue::from_static("websocket"));
    forwarded.insert("sec-websocket-version", HeaderValue::from_static("13"));
    forwarded.insert(
        "sec-websocket-key",
        HeaderValue::from_str(&key).map_err(|_| failure)?,
    );
    if relay.is_draining() {
        return Err(failure);
    }
    let response = tokio::time::timeout_at(
        deadline,
        relay
            .client
            .get(&context.upstream_url)
            .headers(forwarded)
            .send(),
    )
    .await
    .map_err(|_| failure)?
    .map_err(|_| failure)?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS
        || response
            .headers()
            .get("sec-websocket-accept")
            .is_none_or(|value| value.as_bytes() != derive_accept_key(key.as_bytes()).as_bytes())
        || !header_token(response.headers(), "upgrade", "websocket")
        || !header_token(response.headers(), "connection", "upgrade")
        || response.headers().contains_key("sec-websocket-extensions")
        || response.headers().contains_key("sec-websocket-protocol")
    {
        return Err(failure);
    }
    if let Some(key) = context.session.key.as_deref() {
        relay.observability.dashboard().response_model(
            key,
            response
                .headers()
                .get("openai-model")
                .and_then(|v| v.to_str().ok()),
        );
    }
    let stream = tokio::time::timeout_at(deadline, response.upgrade())
        .await
        .map_err(|_| failure)?
        .map_err(|_| failure)?;
    context
        .guard_launch_model(model, deadline)
        .await
        .map_err(|error| error.code())?;
    if !relay
        .dispatch_data_allowed(context.session.key.as_deref(), &account.id, model)
        .await
    {
        return Err("routing_policy_blocked");
    }
    let limit = mib_bytes(relay.websocket.max_message_size_mib);
    let config = WebSocketConfig::default()
        .max_message_size(Some(limit))
        .max_frame_size(Some(limit))
        .write_buffer_size(0);
    Ok(WebSocketStream::from_raw_socket(stream, Role::Client, Some(config)).await)
}

pub(super) async fn upgrade(state: Arc<RelayState>, request: Request) -> Response {
    let mut completion = Completion::new(
        state.observability.clone(),
        &Method::GET,
        "/v1/responses",
        Provider::Codex,
        request.headers(),
    );
    completion.dashboard_enabled = false;
    completion.record.transport = "ws".into();
    completion.track(state.inflight_requests.clone());
    completion.runtime = Some(state.accounts.clone());
    let deadline = request
        .extensions()
        .get::<RequestDeadline>()
        .map(|d| d.0)
        .unwrap_or_else(|| {
            std::time::Instant::now() + Duration::from_secs(state.ws_upgrade_hold_secs)
        });
    completion.precommit_deadline = Some(deadline);
    completion.record.timeout_phase = Some("launch_resolution".into());
    let timeout = deadline.saturating_duration_since(std::time::Instant::now());
    let response = tokio::time::timeout(timeout, async {
        tokio::select! {
            biased;
            _ = state.wait_for_drain_start() => handover_response(),
            response = upgrade_inner(state.clone(), request, &mut completion, deadline) => response,
        }
    })
    .await
    .unwrap_or_else(|_| {
        native_error(
            StatusCode::GATEWAY_TIMEOUT,
            Provider::Codex,
            "request_timeout",
            "WebSocket 연결의 전체 대기 한도를 초과했습니다.",
        )
    });
    completion.record.status = response.status().as_u16();
    if response.status() != StatusCode::GATEWAY_TIMEOUT {
        completion.record.timeout_phase = None;
    }
    let local_rate_timeout = completion.record.outcome == "rate_wait_timeout";
    completion.finish(if local_rate_timeout {
        "rate_wait_timeout"
    } else if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        "ws_upgrade"
    } else if response.status() == StatusCode::GATEWAY_TIMEOUT {
        "request_timeout"
    } else {
        "ws_upgrade_error"
    });
    response
}

async fn upgrade_inner(
    state: Arc<RelayState>,
    mut request: Request,
    completion: &mut Completion,
    deadline: std::time::Instant,
) -> Response {
    if let Some(response) = state.memory_rejection(Provider::Codex) {
        return response;
    }
    use base64::{Engine, engine::general_purpose::STANDARD};
    let key = request
        .headers()
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok());
    let valid = key
        .and_then(|v| STANDARD.decode(v).ok())
        .is_some_and(|v| v.len() == 16)
        && header_token(request.headers(), "connection", "upgrade")
        && request
            .headers()
            .get("sec-websocket-version")
            .is_some_and(|v| v == "13");
    if !valid {
        return native_error(
            StatusCode::BAD_REQUEST,
            Provider::Codex,
            "invalid_request_error",
            "Invalid WebSocket upgrade",
        );
    }
    let client_accept = derive_accept_key(key.expect("validated key").as_bytes());
    let client_upgrade = hyper::upgrade::on(&mut request);
    let launcher_label = request
        .extensions_mut()
        .remove::<crate::dashboard::labels::IngressLabel>()
        .and_then(|e| e.0);
    let (parts, _) = request.into_parts();
    let ingress = parts.extensions.get::<launch::LaunchIngress>().cloned();
    let launch_pin = ingress.as_ref().and_then(|ingress| ingress.pin.clone());
    let mut session = session::classify(Provider::Codex, "/v1/responses", &parts.headers, &[]);
    let launch_deadline = tokio::time::Instant::from_std(deadline);
    let managed = match state
        .resolve_data_launch(ingress.as_ref(), session.key.as_deref(), launch_deadline)
        .await
    {
        Ok(Some(info)) => {
            session.key = Some(info.canonical_key.clone());
            if info.managed {
                // The upgrade has no body model. Use the effective launch model;
                // each response.create is admitted separately by the pump.
                completion.record.timeout_phase = Some("source_model_admission".into());
                match state.admit_data_model(&info, None, launch_deadline).await {
                    Ok(Some(model)) => {
                        session.model = Some(model.clone());
                        Some(http_data::ManagedHttp { info, model })
                    }
                    Ok(None) => {
                        return http_data::error(completion, launch::DataLaunchError::Invalid);
                    }
                    Err(error) => return http_data::error(completion, error),
                }
            } else {
                None
            }
        }
        Ok(None) => None,
        Err(error) => return http_data::error(completion, error),
    };
    completion.record.timeout_phase = Some("project_admission".into());
    if state
        .prepare_project(
            &session,
            parts.extensions.get::<crate::routing::ProjectHint>(),
        )
        .await
        .is_err()
    {
        return super::native_error(
            StatusCode::CONFLICT,
            Provider::Codex,
            "routing_project_conflict",
            "등록된 프로젝트와 루트 세션 연결을 확인해 주세요.",
        );
    }
    // A reconnect upgrade has no body model. Retain the model from the actual
    // failed precommit turn so model-scoped health and evidence stay scoped.
    if session.model.is_none() {
        let source = state.bindings.current(session.key.as_deref());
        session.model = state.reconnect_model(session.key.as_deref(), source.as_deref());
    }
    completion.session(&session);
    let ticket = state.observability.capture_request(&parts.headers, &[]);
    let query = parts
        .uri
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let url = format!("{}/responses{query}", state.codex_base);
    let limit = mib_bytes(state.websocket.max_message_size_mib);
    let config = WebSocketConfig::default()
        .max_message_size(Some(limit))
        .max_frame_size(Some(limit))
        .write_buffer_size(0);
    let mut tried = std::collections::BTreeSet::new();
    let mut forbidden = 0;
    let mut last_error = None;
    // One pacing deadline for the entire upgrade, including retries and
    // account selection. C7 reset-hold and token/network budgets stay separate.
    // Leave a small envelope for the hold's final forced attempt before the
    // independent overall connect deadline cancels the future.
    let upgrade_deadline = deadline
        .checked_sub(Duration::from_millis(100))
        .unwrap_or(deadline);
    let mut quota_source: Option<String> = None;
    let mut own_wait: Option<capacity::OwnWait> = None;
    let mut capacity_wait_deadline = None;
    let (upstream, upstream_headers, account, tokens, capacity_permit) = 'accounts: loop {
        if tried.len() == 4 {
            if let Some(id) = quota_source.take() {
                state.notice_quota(&id, None, None).await;
            }
            return match last_error {
                Some(response) => response,
                None => {
                    state
                        .unavailable(completion, Provider::Codex, true, None)
                        .await
                }
            };
        }
        // Recovery authority precedes any capacity queue. The source remains
        // only a hint; final selection and spill admission revalidate it.
        completion.record.timeout_phase = Some("binding_authority".into());
        let binding_hint = {
            let table = state.bindings.clone();
            let key = session.key.clone();
            tokio::task::spawn_blocking(move || {
                let policy = table.routing_plan(key.as_deref());
                if policy.settings.mode == crate::routing::Mode::Strict
                    && (table.policies.strict_dispatch_blocked()
                        || policy
                            .anchor
                            .as_deref()
                            .is_some_and(|anchor| !table.permits_dispatch(key.as_deref(), anchor)))
                {
                    return Err(());
                }
                Ok(table.capacity_source(key.as_deref()))
            })
        };
        let source = match tokio::select! {
            biased;
            _ = tokio::time::sleep_until(launch_deadline) => return capacity_timeout(),
            _ = state.wait_for_drain_start() => return handover_response(),
            result = binding_hint => result,
        } {
            Ok(Ok(source)) => source,
            Ok(Err(())) => {
                completion.policy_blocked = true;
                return state
                    .unavailable(completion, Provider::Codex, true, None)
                    .await;
            }
            Err(_) => return network_failure(),
        };
        let mut capped = state.ws_capacity.capped();
        if own_wait.as_ref().is_some_and(|wait| {
            source.as_deref() != Some(wait.source()) || tried.contains(wait.source())
        }) {
            own_wait = None;
        }
        if own_wait.is_none()
            && let Some(source) = source.as_deref().filter(|source| !tried.contains(*source))
        {
            own_wait = capacity::OwnWait::start(&state.ws_capacity, source, upgrade_deadline);
            if own_wait.is_none() {
                // A release between the snapshot and start must not exclude the
                // source after it already regained its own slot.
                capped.remove(source);
            }
        }
        let spill_source = own_wait
            .as_ref()
            .and_then(|wait| wait.spill_source(&state.ws_capacity));
        if let Some(wait) = own_wait.as_mut()
            && capped.contains(wait.source())
            && spill_source.is_none()
        {
            completion.end_attempt();
            completion.record.timeout_phase = Some("source_capacity".into());
            wait.wait_once(&state.ws_capacity).await;
            continue;
        }
        // A slot returned during the grace gets one source-only native choice,
        // including in Rebalance. This changes eligibility, never authority.
        let preferred_source = own_wait
            .as_ref()
            .filter(|wait| !capped.contains(wait.source()))
            .map(|wait| wait.source().to_owned());
        // A local cap is not evidence that a managed launch's account failed.
        // Keep its healthy binding and let the reservation refusal wait below.
        let mut excluded = if managed.is_some() {
            tried.clone()
        } else {
            tried.union(&capped).cloned().collect()
        };
        if let Some(source) = &preferred_source {
            let candidates = match tokio::select! {
                biased;
                _ = tokio::time::sleep_until(launch_deadline) => return capacity_timeout(),
                _ = state.wait_for_drain_start() => return handover_response(),
                result = state.candidates(
                    Provider::Codex, session.key.as_deref(), session.model.as_deref(), &tried,
                ) => result,
            } {
                Ok(candidates) => candidates,
                Err(_) => return network_failure(),
            };
            excluded.extend(candidates.into_iter().filter_map(|candidate| {
                (candidate.account.id != *source).then_some(candidate.account.id)
            }));
        }
        let reserved = Arc::new(std::sync::Mutex::new(
            None::<(
                capacity::Permit,
                Option<crate::bindings::CapacitySpillPermit>,
            )>,
        ));
        let admission_denied = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let own_wait_exhausted = spill_source.is_some();
        let admit = {
            let state = state.clone();
            // The blocking selector must not keep a cancelled upgrade alive.
            // Only this live future owns the strong reservation container.
            let owner = Arc::downgrade(&reserved);
            let denied = admission_denied.clone();
            let key = session.key.clone();
            let preferred = preferred_source.clone();
            let failed = tried.clone();
            Arc::new(move |account: &str| {
                #[cfg(feature = "testkit")]
                state.ws_capacity.before_admission();
                if state.is_draining() || std::time::Instant::now() >= deadline {
                    return None;
                }
                let reserved = owner.upgrade()?;
                if let Some(source) = &preferred
                    && (account != source
                        || state.bindings.capacity_source(key.as_deref()).as_deref()
                            != Some(source.as_str()))
                {
                    denied.store(true, Ordering::Release);
                    return None;
                }
                let Some(permit) = state.ws_capacity.reserve(account) else {
                    denied.store(true, Ordering::Release);
                    return None;
                };
                let current_source = state.bindings.capacity_source(key.as_deref());
                if let Some(source) = current_source.as_deref()
                    && source != account
                    && !failed.contains(source)
                    && state.ws_capacity.is_capped(source)
                    && spill_source
                        .as_ref()
                        .is_none_or(|proof| proof.source() != source)
                {
                    // A cap/binding change after collection cannot manufacture
                    // an exhausted own-wait for the newly observed source.
                    denied.store(true, Ordering::Release);
                    return None;
                }
                // Capacity is reserved before the atomic spill fence, and both
                // reservations roll back if publication/caller ownership fails.
                let (spill, admission) = if let Some(source) = &spill_source
                    && account != source.source()
                {
                    let Some(proof) = source.admit(&permit) else {
                        denied.store(true, Ordering::Release);
                        return None;
                    };
                    let spill = match state.bindings.admit_capacity_spill(
                        key.as_deref(),
                        source.source(),
                        account,
                    ) {
                        Ok(spill) => spill,
                        Err(()) => {
                            denied.store(true, Ordering::Release);
                            return None;
                        }
                    };
                    (
                        Some(spill),
                        crate::bindings::DispatchAdmission::capacity(proof),
                    )
                } else {
                    (None, crate::bindings::DispatchAdmission::ordinary())
                };
                let mut reserved = reserved.lock().unwrap_or_else(|e| e.into_inner());
                if state.is_draining() || std::time::Instant::now() >= deadline {
                    return None;
                }
                *reserved = Some((permit, spill));
                Some(admission)
            })
        };
        completion.policy_blocked = false;
        completion.record.timeout_phase = Some("account_selection".into());
        let selected = state
            .select_launch_request_admitted(
                Provider::Codex,
                &session,
                completion,
                session.model.as_deref(),
                &excluded,
                managed.as_ref(),
                Some(admit),
            )
            .await;
        if state.is_draining() {
            return handover_response();
        }
        if let Some(id) = quota_source.take()
            && let Ok(next) = &selected
        {
            state.notice_quota(&id, next.as_ref(), None).await;
        }
        if !matches!(selected, Ok(Some(_))) {
            // Policy persistence may reject after admission. Never keep that
            // unused permit while deciding whether another candidate can wait.
            reserved.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
        let account = match selected {
            Ok(Some(account)) => account,
            Ok(None) => {
                let policy = state.bindings.routing_plan(session.key.as_deref());
                if policy.settings.mode == crate::routing::Mode::Strict
                    && (state.bindings.policies.strict_dispatch_blocked()
                        || policy.anchor.as_deref().is_some_and(|anchor| {
                            !state
                                .bindings
                                .permits_dispatch(session.key.as_deref(), anchor)
                        }))
                {
                    completion.policy_blocked = true;
                    return state
                        .unavailable(completion, Provider::Codex, true, None)
                        .await;
                }
                if preferred_source.is_some()
                    && !admission_denied.load(Ordering::Acquire)
                    && !completion.policy_blocked
                {
                    // The returned slot did not make its source natively
                    // eligible. Recollect through the ordinary policy path.
                    own_wait = None;
                    completion.end_attempt();
                    continue;
                }
                // Preview the actual routing decision with only capacity
                // exclusions removed. In particular, unrelated full accounts
                // cannot mask an unavailable strict anchor or drain policy.
                let capacity_blocked = admission_denied.load(Ordering::Acquire)
                    || (managed.is_none()
                        && !capped.is_empty()
                        && capacity_candidate(&state, &session, &tried, &capped).await);
                if capacity_blocked {
                    completion.end_attempt();
                    completion.record.timeout_phase = Some("destination_capacity".into());
                    // An exhausted own-slot grace already permits the ordinary
                    // guarded spill attempt above. If it cannot land, HTTP can
                    // still serve this binding without a socket reservation.
                    // Unbound/racing admissions get one short queue too; never
                    // renew it on each selection or consume the connect budget.
                    let wait_deadline = *capacity_wait_deadline.get_or_insert_with(|| {
                        let now = std::time::Instant::now();
                        if own_wait_exhausted {
                            now
                        } else {
                            now + (upgrade_deadline.saturating_duration_since(now) / 2)
                                .min(Duration::from_millis(250))
                        }
                    });
                    if state.ws_capacity.wait_until(wait_deadline).await {
                        continue;
                    }
                    return capacity_http_fallback();
                }
                if completion.policy_blocked {
                    return state
                        .unavailable(completion, Provider::Codex, true, None)
                        .await;
                }
                return match last_error {
                    Some(response) => response,
                    None => {
                        state
                            .unavailable(completion, Provider::Codex, true, None)
                            .await
                    }
                };
            }
            Err(_) => return network_failure(),
        };
        let (capacity_permit, spill_permit) = reserved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("selected WS account has an admitted slot");
        if let Some(spill) = spill_permit {
            spill.commit();
        }
        tried.insert(account.id.clone());
        completion.record.timeout_phase = Some("credential_gate".into());
        let (mut account, mut tokens) = match state
            .tokens
            .before_request(state.store.clone(), state.observability.clone(), account.id)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(_) => return network_failure(),
        };
        if !state
            .accounts
            .eligible(&account.id, session.model.as_deref())
            .await
        {
            continue;
        }
        let mut probe = match state.accounts.begin_probe(&account) {
            Ok(probe) => probe,
            Err(_) => continue,
        };
        // Row 5 and row 11 have independent same-account retry budgets.
        let mut network_retry = false;
        let mut token_retry = false;
        let mut rate_retries = 0_u32;
        let mut rate_retry_queued = false;
        loop {
            let admission_started = std::time::Instant::now();
            completion.record.timeout_phase = Some("rate_admission".into());
            let admission = state
                .accounts
                .admit_until(&account.id, upgrade_deadline)
                .await;
            completion.rate_wait(
                u64::try_from(admission_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            );
            let mut forced = admission.forced() || std::time::Instant::now() >= upgrade_deadline;
            completion.record.timeout_phase = Some("final_model_admission".into());
            if let Some(managed) = &managed
                && let Err(error) = state
                    .guard_http_launch(managed, &account.id, launch_deadline)
                    .await
            {
                return http_data::error(completion, error);
            }
            completion.record.timeout_phase = Some("dispatch_admission".into());
            if !state
                .dispatch_data_allowed(
                    session.key.as_deref(),
                    &account.id,
                    session.model.as_deref(),
                )
                .await
            {
                completion.policy_blocked = !state
                    .bindings
                    .permits_dispatch(session.key.as_deref(), &account.id);
                return state
                    .unavailable(completion, Provider::Codex, true, None)
                    .await;
            }
            if state.is_draining() {
                return handover_response();
            }
            let mut forwarded =
                match headers::upstream(&parts.headers, &account, &tokens.access_token) {
                    Ok(headers) => headers,
                    Err(_) => return network_failure(),
                };
            let key = generate_key();
            forwarded.insert("connection", HeaderValue::from_static("Upgrade"));
            forwarded.insert("upgrade", HeaderValue::from_static("websocket"));
            forwarded.insert("sec-websocket-version", HeaderValue::from_static("13"));
            forwarded.insert(
                "sec-websocket-key",
                HeaderValue::from_str(&key).expect("generated key"),
            );
            completion.record.account = Some(account.id.clone());
            if std::time::Instant::now() >= deadline {
                return native_error(
                    StatusCode::GATEWAY_TIMEOUT,
                    Provider::Codex,
                    "request_timeout",
                    "WebSocket 연결 시간 한도를 초과했습니다.",
                );
            }
            forced |= std::time::Instant::now() >= upgrade_deadline;
            if std::mem::take(&mut rate_retry_queued) {
                completion.rate_retry();
            }
            if let Some(binding) = &completion.binding {
                binding.dispatched();
            }
            completion.attempt(&account.id);
            if forced {
                state.accounts.record_forced_send(&account.id);
            }
            let observation_asof = state.bindings.begin_usage_observation_for(&account.id);
            let sent_at = std::time::Instant::now();
            completion.record.timeout_phase = Some("upstream_upgrade".into());
            let result = tokio::time::timeout(
                Duration::from_secs(300),
                state.client.get(&url).headers(forwarded).send(),
            )
            .await;
            let response = match result {
                Ok(Ok(response)) => response,
                _ if !network_retry => {
                    completion.failover("network_retry");
                    network_retry = true;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
                _ => return network_failure(),
            };
            completion.record.upstream_status = Some(response.status().as_u16());
            let credit_observation = if response.status() == StatusCode::SWITCHING_PROTOCOLS
                || response.status().is_success()
                || response.status() == StatusCode::TOO_MANY_REQUESTS
            {
                state
                    .accounts
                    .observe_headers_asof(
                        &account.id,
                        Provider::Codex,
                        response.headers(),
                        None,
                        (&state.bindings, observation_asof),
                    )
                    .await
            } else {
                Default::default()
            };
            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                let headers = response.headers().clone();
                if headers
                    .get("sec-websocket-accept")
                    .is_none_or(|v| v.as_bytes() != derive_accept_key(key.as_bytes()).as_bytes())
                    || !header_token(&headers, "upgrade", "websocket")
                    || !header_token(&headers, "connection", "upgrade")
                    || headers.contains_key("sec-websocket-extensions")
                    || headers.contains_key("sec-websocket-protocol")
                {
                    return network_failure();
                }
                let stream = match response.upgrade().await {
                    Ok(stream) => stream,
                    Err(_) => return network_failure(),
                };
                state
                    .accounts
                    .credit_headers(&account.id, &headers, &credit_observation)
                    .await;
                if probe.is_some() && state.accounts.probe_success(&account.id).await.is_err() {
                    return network_failure();
                }
                if let Some(managed) = &managed {
                    completion.record.timeout_phase = Some("final_model_admission".into());
                    if let Err(error) = state
                        .guard_http_launch(managed, &account.id, launch_deadline)
                        .await
                    {
                        return http_data::error(completion, error);
                    }
                    completion.record.timeout_phase = Some("dispatch_admission".into());
                    if !state
                        .dispatch_data_allowed(
                            session.key.as_deref(),
                            &account.id,
                            session.model.as_deref(),
                        )
                        .await
                    {
                        return http_data::error(
                            completion,
                            launch::DataLaunchError::PolicyBlocked,
                        );
                    }
                }
                drop(probe);
                break 'accounts (
                    WebSocketStream::from_raw_socket(stream, Role::Client, Some(config)).await,
                    headers,
                    account,
                    tokens,
                    capacity_permit,
                );
            }
            let error = match read_error(response).await {
                Ok(error) => error,
                Err(_) if !network_retry => {
                    completion.failover("network_retry");
                    network_retry = true;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
                Err(_) => return network_failure(),
            };
            completion.record.upstream_error_code =
                super::hold::error_code(&error.headers, &error.raw);
            if error.status == StatusCode::BAD_REQUEST
                && completion.record.upstream_error_code.as_deref()
                    == Some("websocket_connection_limit_reached")
            {
                capacity_permit.connection_limit();
                // Capacity is independent of health and does not consume the
                // four-account failure budget. RAII releases this rejected dial
                // before the next selection; the learned cap remains.
                tried.remove(&account.id);
                completion.failover("ws_connection_limit");
                continue 'accounts;
            }
            let current_token = if error.status == StatusCode::UNAUTHORIZED {
                match state
                    .tokens
                    .is_current(state.store.clone(), &account, &tokens)
                    .await
                {
                    Ok(current) => current,
                    Err(()) => return network_failure(),
                }
            } else {
                true
            };
            let mut decision = super::outcome::classify(
                Provider::Codex,
                error.status,
                &error.headers,
                &error.raw,
                session.model.as_deref(),
            );
            let failure_evidence = decision.failed_attempt(
                &account,
                session.key.as_deref(),
                session.model.as_deref(),
                deadline,
                &completion.failure_lifetime,
            );
            decision.reconcile_quota(&credit_observation);
            if decision.kind == super::outcome::Kind::Quota {
                state.accounts.invalidate_quota_rejection(&account.id).await;
            }
            if super::credentials::is_linked(&account)
                && decision.kind == super::outcome::Kind::Revoked
            {
                decision.kind = super::outcome::Kind::Auth;
                decision.signal = None;
            }
            if !current_token
                && matches!(
                    decision.kind,
                    super::outcome::Kind::Auth | super::outcome::Kind::Revoked
                )
            {
                decision.kind = super::outcome::Kind::Auth;
                decision.signal = None;
            }
            if decision.kind == super::outcome::Kind::Rate {
                state.bindings.note_rate(&account.id, chrono::Utc::now());
                let retry_after = super::outcome::header_seconds(&error.headers, "retry-after")
                    .map(|seconds| Duration::from_secs(seconds as u64));
                state.accounts.observe_429_at(
                    &account.id,
                    sent_at,
                    std::time::Instant::now(),
                    retry_after,
                    1.0,
                );
                if rate_retries < state.rate_hold_max_retries
                    && !forced
                    && std::time::Instant::now() < upgrade_deadline
                {
                    rate_retries += 1;
                    rate_retry_queued = true;
                    continue;
                }
                state
                    .note_client_rate(
                        crate::accounts::Provider::Codex,
                        &account.id,
                        session.key.as_deref(),
                    )
                    .await;
                completion.relayed_rate_429();
                let mut response = error.into_response();
                if !response.headers().contains_key("retry-after") {
                    response
                        .headers_mut()
                        .insert("retry-after", HeaderValue::from_static("1"));
                }
                return response;
            }
            let failover = decision.failover();
            let reason = decision.reason();
            if decision.kind == super::outcome::Kind::Auth {
                if current_token
                    && (token_retry
                        || token_retried(&state, &account.id, &tokens.access_token, false))
                {
                    if state
                        .tokens
                        .auth_failed(
                            state.store.clone(),
                            state.observability.clone(),
                            account.clone(),
                            &tokens,
                        )
                        .await
                        .is_err()
                    {
                        return network_failure();
                    }
                } else {
                    match state
                        .tokens
                        .after_401(
                            state.store.clone(),
                            state.observability.clone(),
                            account.clone(),
                            &tokens,
                        )
                        .await
                    {
                        Ok(Some((updated, refreshed))) if !token_retry => {
                            completion.failover("token_retry");
                            token_retry = true;
                            token_retried(&state, &updated.id, &refreshed.access_token, true);
                            account = updated;
                            tokens = refreshed;
                            continue;
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            if state
                                .tokens
                                .cool_unavailable_auth(state.store.clone(), &account, &tokens)
                                .await
                                .is_err()
                            {
                                return network_failure();
                            }
                        }
                        Err(()) => return network_failure(),
                    }
                }
            } else if decision.kind == super::outcome::Kind::Revoked {
                if state
                    .tokens
                    .revoke_if_current(state.store.clone(), &account, &tokens)
                    .await
                    .is_err()
                {
                    return network_failure();
                }
            } else if let Some(signal) = decision.signal
                && state
                    .accounts
                    .transition(&account.id, signal)
                    .await
                    .is_err()
            {
                return network_failure();
            }
            if managed.is_some() && decision.kind == super::outcome::Kind::ModelPermission {
                // Keep C6 row 6a's durable model hold above, then refresh only
                // this account. A capability denial never authorizes shopping.
                let _ = tokio::time::timeout_at(
                    launch_deadline,
                    state.launch_catalog(&account.id, true, true),
                )
                .await;
                return error.into_response();
            }
            if failover {
                completion.failure_evidence = failure_evidence;
                completion.failover(reason);
            }
            if error.complete
                && matches!(
                    decision.kind,
                    super::outcome::Kind::Quota | super::outcome::Kind::Rate
                )
            {
                state
                    .accounts
                    .cache_limit(&account.id, error.status, &error.headers, &error.raw);
            }
            if error.status == StatusCode::FORBIDDEN {
                forbidden += 1;
            }
            if decision.kind == super::outcome::Kind::Quota {
                state
                    .accounts
                    .observe_quota_reset(&account.id, decision.quota_reset);
                drop(probe.take());
                match state
                    .retry_after_reset(&session, completion, &account.id, None)
                    .await
                {
                    Ok(true) => {
                        match state
                            .tokens
                            .before_request(
                                state.store.clone(),
                                state.observability.clone(),
                                account.id.clone(),
                            )
                            .await
                        {
                            Ok((updated, refreshed)) => {
                                account = updated;
                                tokens = refreshed;
                            }
                            Err(()) => return network_failure(),
                        }
                        probe = match state.accounts.begin_probe(&account) {
                            Ok(probe) => probe,
                            Err(_) => continue 'accounts,
                        };
                        continue;
                    }
                    Ok(false) => {
                        quota_source = Some(account.id.clone());
                    }
                    Err(()) => return network_failure(),
                }
            }
            let preserve_last = matches!(
                decision.kind,
                super::outcome::Kind::Payment | super::outcome::Kind::ModelPermission
            );
            let response = error.into_response();
            if !failover || forbidden == 2 {
                return response;
            }
            last_error = preserve_last.then_some(response);
            continue 'accounts;
        }
    };
    state
        .observability
        .capture_ws_response(ticket.as_deref(), &upstream_headers);
    if let Some(key) = session.key.as_deref() {
        state
            .observability
            .dashboard()
            .evidence(key, &parts.headers, launcher_label);
        state.observability.dashboard().response_model(
            key,
            upstream_headers
                .get("openai-model")
                .and_then(|v| v.to_str().ok()),
        );
    }
    let forwarded = headers::filtered(&upstream_headers, false);
    let close_after_response = {
        #[cfg(all(feature = "phase0", debug_assertions))]
        {
            state.close_after_response
        }
        #[cfg(not(all(feature = "phase0", debug_assertions)))]
        {
            false
        }
    };
    let socket_pin = state.bindings.pin(session.key.as_deref());
    if let Some(lease) = &mut completion.binding {
        // A reset retry replaces the attempt lease, but the handshake still
        // owes its accumulated move marker to the first response.create.
        lease.after_move |= completion.record.after_move;
        lease.defer_after_move();
    }
    // The handshake defers the marker to the first response.create ledger row.
    completion.record.after_move = false;
    let context = pump::Context {
        runtime: Some(state.accounts.clone()),
        relay: Some(state.clone()),
        drain_signal: Some(Box::pin({
            let state = state.clone();
            async move { state.wait_for_drain_start().await }
        })),
        credentials: Some((
            account.clone(),
            zeroize::Zeroizing::new(tokens.access_token.clone()),
        )),
        budget: state.budget.clone(),
        memory_guard: state.memory_guard.clone(),
        observability: state.observability.clone(),
        account: account.id.clone(),
        session,
        headers: parts.headers,
        upstream_url: url,
        idle_close: Duration::from_secs(state.websocket.idle_close_secs),
        close_after_response,
        capture_ticket: ticket,
        connection: format!("{:032x}", rand::random::<u128>()),
    };
    struct SocketActivity(Arc<std::sync::atomic::AtomicU64>);
    impl Drop for SocketActivity {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }
    state.inflight_requests.fetch_add(1, Ordering::AcqRel);
    let activity = SocketActivity(state.inflight_requests.clone());
    let dashboard_socket = context
        .session
        .key
        .as_deref()
        .and_then(|key| state.observability.dashboard().socket(key));
    tokio::spawn(async move {
        let _launch_pin = launch_pin;
        // The slot survives the HTTP 101 and is released on client-upgrade
        // cancellation, pump termination or task abort. Same-account redials
        // retain this logical socket's slot while replacing its closed leg.
        let _capacity_permit = capacity_permit;
        let _activity = activity;
        let _dashboard_socket = dashboard_socket;
        let _socket_pin = socket_pin;
        if let Ok(client) = client_upgrade.await {
            let client = WebSocketStream::from_raw_socket(
                hyper_util::rt::TokioIo::new(client),
                Role::Server,
                Some(config),
            )
            .await;
            pump::run(client, upstream, context).await;
        }
    });
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    response
        .headers_mut()
        .insert("connection", HeaderValue::from_static("Upgrade"));
    response
        .headers_mut()
        .insert("upgrade", HeaderValue::from_static("websocket"));
    response.headers_mut().insert(
        "sec-websocket-accept",
        HeaderValue::from_str(&client_accept).expect("generated accept"),
    );
    for (name, value) in &forwarded {
        response.headers_mut().append(name.clone(), value.clone());
    }
    if let Ok(value) = HeaderValue::from_str(&account.id) {
        response
            .headers_mut()
            .insert("x-quota-baton-account", value);
    }
    response
}

async fn capacity_candidate(
    state: &RelayState,
    session: &session::Session,
    tried: &BTreeSet<String>,
    capped: &BTreeSet<String>,
) -> bool {
    if session.key.is_none()
        && state.bindings.routing_plan(None).settings.mode == crate::routing::Mode::Strict
    {
        return false;
    }
    let Ok(candidates) = state
        .candidates(Provider::Codex, session.key.as_deref(), None, tried)
        .await
    else {
        return false;
    };
    matches!(state.bindings.preview(
        Provider::Codex, session.key.as_deref(), session.parent.as_deref(), None,
        &candidates, state.pins.get(Provider::Codex, session.key.as_deref()).as_deref(), 0,
    ), crate::bindings::PreviewDecision::Selected { account } if capped.contains(&account))
}

fn handover_response() -> Response {
    let mut response = native_error(
        StatusCode::SERVICE_UNAVAILABLE,
        Provider::Codex,
        "server_error",
        "quota-baton is handing over",
    );
    response
        .headers_mut()
        .insert("retry-after", HeaderValue::from_static("1"));
    response
}

fn capacity_http_fallback() -> Response {
    // Codex 0.163/0.164 core/src/client.rs handles an upgrade HTTP 426
    // as FallbackToHttp, immediately and for the rest of the session.
    // 0.163: lines 1961-1964, 2295-2314:
    // https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/core/src/client.rs#L1961
    // 0.164: lines 1999-2002, 2327-2346:
    // https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/core/src/client.rs#L1999
    native_error(
        StatusCode::UPGRADE_REQUIRED,
        Provider::Codex,
        "websocket_capacity",
        "WebSocket capacity is unavailable; retry this request over HTTP.",
    )
}

fn capacity_timeout() -> Response {
    native_error(
        StatusCode::GATEWAY_TIMEOUT,
        Provider::Codex,
        "request_timeout",
        "WebSocket 연결 용량 대기 한도를 초과했습니다.",
    )
}

fn network_failure() -> Response {
    native_error(
        StatusCode::BAD_GATEWAY,
        Provider::Codex,
        "server_error",
        "Could not connect to upstream WebSocket",
    )
}

fn header_token(headers: &HeaderMap, name: &str, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case(token))
}

struct HandshakeError {
    status: StatusCode,
    headers: HeaderMap,
    raw: Vec<u8>,
    remainder: Option<Bytes>,
    stream:
        std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    complete: bool,
}

async fn read_error(response: reqwest::Response) -> Result<HandshakeError, ()> {
    use futures_util::StreamExt;
    let mut error = HandshakeError {
        status: response.status(),
        headers: response.headers().clone(),
        raw: Vec::new(),
        remainder: None,
        stream: Box::pin(response.bytes_stream()),
        complete: false,
    };
    // Read beyond the bound to distinguish an exact 64 KiB complete body.
    loop {
        match tokio::time::timeout(Duration::from_secs(10), error.stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                let take = chunk.len().min(65536 - error.raw.len());
                error.raw.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    error.remainder = Some(chunk.slice(take..));
                    break;
                }
            }
            Ok(None) => {
                error.complete = true;
                break;
            }
            _ => return Err(()),
        }
    }
    Ok(error)
}

impl HandshakeError {
    fn into_response(self) -> Response {
        use futures_util::StreamExt;
        if self.status == StatusCode::UPGRADE_REQUIRED {
            return network_failure();
        }
        let Self {
            status,
            headers: response_headers,
            raw,
            remainder,
            mut stream,
            ..
        } = self;
        let output = async_stream::stream! {
            if !raw.is_empty() { yield Ok::<Bytes, std::io::Error>(raw.into()); }
            if let Some(chunk) = remainder { yield Ok(chunk); }
            while let Some(chunk) = stream.next().await {
                yield chunk.map_err(|_| std::io::Error::other("Upstream handshake body interrupted"));
            }
        };
        let mut response = Response::new(axum::body::Body::from_stream(output));
        *response.status_mut() = status;
        *response.headers_mut() = headers::filtered(&response_headers, false);
        response
    }
}
