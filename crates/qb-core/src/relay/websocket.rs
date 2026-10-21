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

pub(super) type UpstreamSocket = WebSocketStream<reqwest::Upgraded>;
pub(super) type ClientSocket = WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>;

async fn redial_same_account(
    context: &pump::Context,
    model: Option<&str>,
) -> Result<UpstreamSocket, ()> {
    let relay = context.relay.as_ref().ok_or(())?;
    let (account, token) = context.credentials.as_ref().ok_or(())?;
    if !relay
        .dispatch_data_allowed(context.session.key.as_deref(), &account.id, model)
        .await
    {
        return Err(());
    }
    let mut forwarded = headers::upstream(&context.headers, account, token).map_err(|_| ())?;
    let key = generate_key();
    forwarded.insert("connection", HeaderValue::from_static("Upgrade"));
    forwarded.insert("upgrade", HeaderValue::from_static("websocket"));
    forwarded.insert("sec-websocket-version", HeaderValue::from_static("13"));
    forwarded.insert(
        "sec-websocket-key",
        HeaderValue::from_str(&key).map_err(|_| ())?,
    );
    let response = tokio::time::timeout(
        Duration::from_secs(15),
        relay
            .client
            .get(&context.upstream_url)
            .headers(forwarded)
            .send(),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
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
        return Err(());
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
    let stream = response.upgrade().await.map_err(|_| ())?;
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
    let timeout = deadline.saturating_duration_since(std::time::Instant::now());
    let response = tokio::time::timeout(
        timeout,
        upgrade_inner(state, request, &mut completion, deadline),
    )
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
    let session = session::classify(Provider::Codex, "/v1/responses", &parts.headers, &[]);
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
        // Preserve ordinary sticky selection while there is room. Capacity is
        // a WS-only exclusion: normal routing may move to another eligible
        // account; strict anchors stay anchored and wait for their slot.
        let capped = state.ws_capacity.capped();
        let excluded = tried.union(&capped).cloned().collect();
        completion.policy_blocked = false;
        let selected = state
            .select_request(Provider::Codex, &session, completion, None, &excluded)
            .await;
        if let Some(id) = quota_source.take()
            && let Ok(next) = &selected
        {
            state.notice_quota(&id, next.as_ref(), None).await;
        }
        let account = match selected {
            Ok(Some(account)) => account,
            Ok(None) => {
                // Do not turn capacity exhaustion into quota/account failure.
                // Check unexcluded candidates so unrelated unavailable accounts
                // do not cause a wait merely because some other account is full.
                if !capped.is_empty()
                    && state
                        .candidates(Provider::Codex, session.key.as_deref(), None, &tried)
                        .await
                        .is_ok_and(|candidates| {
                            candidates.iter().any(|candidate| {
                                capped.contains(&candidate.account.id)
                                    && candidate.account.eligible_at(None, chrono::Utc::now())
                            })
                        })
                {
                    completion.end_attempt();
                    if state.ws_capacity.wait_until(deadline).await {
                        continue;
                    }
                    return capacity_timeout();
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
        // Selection awaits disk/routing work, so its snapshot is not a slot.
        // Reserve atomically before refresh/dial and reselect if another
        // upgrade took the final slot in the meantime.
        let Some(capacity_permit) = state.ws_capacity.reserve(&account.id) else {
            completion.end_attempt();
            if state.ws_capacity.wait_until(deadline).await {
                continue;
            }
            return capacity_timeout();
        };
        tried.insert(account.id.clone());
        let (mut account, mut tokens) = match state
            .tokens
            .before_request(state.store.clone(), state.observability.clone(), account.id)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(_) => return network_failure(),
        };
        if !state.accounts.eligible(&account.id, None).await {
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
            let admission = state
                .accounts
                .admit_until(&account.id, upgrade_deadline)
                .await;
            completion.rate_wait(
                u64::try_from(admission_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            );
            let mut forced = admission.forced() || std::time::Instant::now() >= upgrade_deadline;
            if !state
                .dispatch_allowed(session.key.as_deref(), &account.id, None)
                .await
            {
                completion.policy_blocked = !state
                    .bindings
                    .policies
                    .permits_dispatch(session.key.as_deref(), &account.id);
                return state
                    .unavailable(completion, Provider::Codex, true, None)
                    .await;
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
                None,
            );
            decision.reconcile_quota(&credit_observation);
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
            if failover {
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
