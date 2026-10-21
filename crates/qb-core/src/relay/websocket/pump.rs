use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};

use axum::http::{HeaderMap, Method};
use futures_util::{
    Sink, SinkExt, StreamExt,
    future::BoxFuture,
    stream::{SplitSink, SplitStream},
};
use serde_json::Value;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{
    Message,
    protocol::{
        CloseFrame,
        frame::{
            Frame,
            coding::{Data, OpCode},
        },
    },
};

use super::{ClientSocket, UpstreamSocket};
use crate::{
    accounts::Provider::Codex,
    guard::{InflightBudget, Reservation},
    observability::{Observability, safe_error_code, safe_identifier},
    relay::ledger::Completion,
    session::Session,
};

const PARSE_LIMIT: usize = 1024 * 1024;
const TYPE_LIMIT: usize = 100;
const UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(600);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct Context {
    pub runtime: Option<Arc<crate::account_runtime::AccountRuntime>>,
    pub relay: Option<Arc<crate::relay::RelayState>>,
    pub drain_signal:
        Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>>,
    pub credentials: Option<(crate::accounts::Account, zeroize::Zeroizing<String>)>,
    pub budget: Arc<InflightBudget>,
    pub memory_guard: Arc<crate::guard::MemoryGuard>,
    pub observability: Arc<Observability>,
    pub account: String,
    pub session: Session,
    pub headers: HeaderMap,
    pub upstream_url: String,
    pub idle_close: Duration,
    pub close_after_response: bool,
    pub capture_ticket: Option<PathBuf>,
    pub connection: String,
}

pub(in crate::relay) struct CurrentCompletion {
    completion: Completion,
    committed: bool,
    first_message: bool,
    model: Option<String>,
    response_id: Option<String>,
    last_create: Option<(Message, Reservation)>,
    rate: Arc<RateBudget>,
}

impl CurrentCompletion {
    async fn new(context: &Context) -> Option<Self> {
        let mut completion = Completion::new(
            context.observability.clone(),
            &Method::GET,
            "/v1/responses",
            Codex,
            &context.headers,
        );
        completion.record.transport = "ws".into();
        completion.session(&context.session);
        completion.runtime = context.runtime.clone();
        if let Some(relay) = &context.relay {
            completion.track(relay.inflight_requests.clone());
            completion.binding = Some(
                relay
                    .bindings
                    .begin_request_async(context.session.key.as_deref(), &context.account)
                    .await
                    .ok()?,
            );
        }
        completion.record.transport = "ws".into();
        completion.record.after_move = completion
            .binding
            .as_ref()
            .is_some_and(|lease| lease.after_move);
        completion.record.account = Some(context.account.clone());
        completion.record.status = 101;
        completion.record.upstream_status = Some(101);
        completion.attempt(&context.account);
        let rate = Arc::new(RateBudget::new(context));
        *rate.dashboard.lock().unwrap_or_else(|e| e.into_inner()) = completion
            .dashboard_request
            .as_ref()
            .map(|guard| guard.pacing_source());
        Some(Self {
            completion,
            committed: false,
            first_message: true,
            model: None,
            response_id: None,
            last_create: None,
            rate,
        })
    }

    fn upstream(&mut self, event: Option<&Value>) -> Option<&'static str> {
        if let Some(usage) = event.and_then(crate::observability::cache_usage) {
            self.completion.record.cache_usage = Some(usage);
        }
        if self.first_message {
            self.completion.record.ttfb_ms = self.completion.elapsed_ms();
            self.first_message = false;
        }
        let kind = event
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str);
        let error = matches!(kind, Some("error" | "response.failed"))
            || (matches!(kind, Some("response.done" | "response.completed"))
                && event.is_some_and(|event| event["response"]["status"] == "failed"));
        if error {
            self.completion.record.upstream_error_code = event.and_then(safe_error_code);
            if let Some(status) = event
                .and_then(|event| event.get("status"))
                .and_then(Value::as_u64)
                .filter(|status| (100..=599).contains(status))
            {
                self.completion.record.status = status as u16;
                self.completion.record.upstream_status = Some(status as u16);
            }
        } else if kind.is_some_and(|kind| kind.starts_with("response.")) {
            self.committed = true;
            self.last_create = None;
        }
        match kind {
            _ if error => Some("upstream_error"),
            Some("response.completed" | "response.done") => Some("ok"),
            _ => None,
        }
    }
}

// One budget belongs to a response.create, including a cancelled open-socket
// wait followed by a same-account redial. std::Instant matches AccountRuntime.
struct RateBudget {
    deadline: std::time::Instant,
    max_retries: u32,
    state: Mutex<RateState>,
    dashboard: Mutex<Option<crate::dashboard::runtime::PacingSource>>,
    committed: std::sync::atomic::AtomicBool,
    commit_changed: tokio::sync::Notify,
}

struct RateState {
    initial_sent: bool,
    usage_asof: Option<crate::bindings::UsageObservationAsOf>,
    sent_at: std::time::Instant,
    retries: u32,
    forced: bool,
    /// A cancelled admission is not a send; its redial still owns one attempt.
    forced_sent: bool,
    waited: Duration,
    waiting_since: Option<std::time::Instant>,
}

impl RateBudget {
    fn new(context: &Context) -> Self {
        let defaults = crate::config::RoutingConfig::default();
        let (secs, max_retries) = context.relay.as_ref().map_or(
            (defaults.rate_hold_secs, defaults.rate_hold_max_retries),
            |relay| (relay.rate_hold_secs, relay.rate_hold_max_retries),
        );
        Self::with_limits(Duration::from_secs(secs), max_retries)
    }

    fn with_limits(hold: Duration, max_retries: u32) -> Self {
        let now = std::time::Instant::now();
        Self {
            dashboard: Mutex::new(None),
            committed: std::sync::atomic::AtomicBool::new(false),
            commit_changed: tokio::sync::Notify::new(),
            deadline: now + hold,
            max_retries,
            state: Mutex::new(RateState {
                initial_sent: false,
                usage_asof: None,
                sent_at: now,
                retries: 0,
                forced: false,
                forced_sent: false,
                waited: Duration::ZERO,
                waiting_since: None,
            }),
        }
    }

    fn is_committed(&self) -> bool {
        self.committed.load(Ordering::Acquire)
    }
    fn commit(&self) {
        self.committed.store(true, Ordering::Release);
        self.commit_changed.notify_waiters();
    }
    fn can_retry(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        !self.is_committed() && !state.forced_sent && state.retries < self.max_retries
    }
    fn can_send(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.forced |= std::time::Instant::now() >= self.deadline;
        !self.is_committed() && !state.forced_sent
    }

    fn initial_sent(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .initial_sent
    }

    async fn admit(
        self: &Arc<Self>,
        runtime: Option<&Arc<crate::account_runtime::AccountRuntime>>,
        account: &str,
    ) -> bool {
        let committed = self.commit_changed.notified();
        tokio::pin!(committed);
        committed.as_mut().enable();
        if self.is_committed() {
            return false;
        }
        if let Some(runtime) = runtime {
            let wait = RateWait::new(self.clone());
            let admission = tokio::select! {
                admission = runtime.admit_until(account, self.deadline) => admission,
                _ = committed => return false,
            };
            self.state.lock().unwrap_or_else(|e| e.into_inner()).forced |= admission.forced();
            drop(wait);
        }
        self.can_send()
    }

    fn note_usage_send(&self, context: &Context) {
        if let Some(relay) = &context.relay {
            let asof = relay.bindings.begin_usage_observation_for(&context.account);
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // Until a response is attributed, the earliest attempt is the only
            // safe lower bound for a delayed, unlabelled rate-limit event.
            state.usage_asof.get_or_insert(asof);
        }
    }

    fn initial_dispatch(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.initial_sent = true;
        state.sent_at = std::time::Instant::now();
        state.forced |= state.sent_at >= self.deadline;
        state.forced_sent |= state.forced;
    }

    fn record_forced_send(
        &self,
        runtime: Option<&Arc<crate::account_runtime::AccountRuntime>>,
        account: &str,
    ) {
        if self.state.lock().unwrap_or_else(|e| e.into_inner()).forced
            && let Some(runtime) = runtime
        {
            runtime.record_forced_send(account);
        }
    }

    fn sent(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.initial_sent = true;
        state.sent_at = std::time::Instant::now();
        state.retries += 1;
        // Also terminal when a runtime-free fixture reaches the deadline.
        state.forced |= state.sent_at >= self.deadline;
        state.forced_sent |= state.forced;
    }
}

struct RateWait(
    Arc<RateBudget>,
    Option<crate::dashboard::runtime::PacingGuard>,
);

impl RateWait {
    fn new(rate: Arc<RateBudget>) -> Self {
        rate.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting_since = Some(std::time::Instant::now());
        let pacing = rate
            .dashboard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|source| source.start());
        Self(rate, pacing)
    }
}

impl Drop for RateWait {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(start) = state.waiting_since.take() {
            state.waited += start.elapsed();
        }
        drop(state);
        drop(self.1.take());
    }
}

impl CurrentCompletion {
    fn finish(mut self, outcome: &str) {
        self.rate.commit();
        self.completion.record.outcome = outcome.into();
    }
}

impl Drop for CurrentCompletion {
    fn drop(&mut self) {
        let state = self.rate.state.lock().unwrap_or_else(|e| e.into_inner());
        // A completion can be dropped before its pending write future. Include
        // the live wait without depending on future/local destruction order.
        let waited = state.waited
            + state
                .waiting_since
                .map_or(Duration::ZERO, |start| start.elapsed());
        self.completion
            .rate_wait(u64::try_from(waited.as_millis()).unwrap_or(u64::MAX));
        // Keep the accepted request's inflight guard while queued, but do not
        // ledger an upstream attempt if its initial write was never polled.
        if !state.initial_sent {
            self.completion.record.attempts = self.completion.record.attempts.saturating_sub(1);
            if !self.completion.record.attempt_details.is_empty() {
                self.completion.record.attempt_details.remove(0);
            }
        }
        if let Some(account) = self.completion.record.account.clone() {
            for _ in 0..state.retries {
                self.completion.attempt(&account);
                self.completion.rate_retry();
            }
        }
        if state.retries > 0 && self.completion.record.outcome == "ok" {
            self.completion.record.outcome = "rate_hold_ok".into();
        }
        if self.completion.record.relayed_rate_429s > 0 {
            self.completion.record.outcome = "rate_hold_relayed_429".into();
        }
    }
}

// Only completion metadata is queued here. Each direction has one active wire
// message; a rate wait also permits one reserved, deferred client message.
// Local IDs keep delivery bookkeeping stable across new creates.
fn response_completion(
    current: &mut BTreeMap<u64, CurrentCompletion>,
    event: Option<&Value>,
) -> Option<u64> {
    let kind = event.and_then(|event| event["type"].as_str());
    let response_id = event.and_then(|event| {
        event["response_id"]
            .as_str()
            .or_else(|| event["response"]["id"].as_str())
    });
    if let Some(response_id) = response_id {
        if let Some((&id, _)) = current.iter().find(|(_, active)| {
            active.rate.initial_sent() && active.response_id.as_deref() == Some(response_id)
        }) {
            return Some(id);
        }
        if kind == Some("response.created") {
            let (&id, active) = current
                .iter_mut()
                .find(|(_, active)| active.rate.initial_sent() && active.response_id.is_none())?;
            active.response_id = Some(response_id.into());
            return Some(id);
        }
        // An explicit unknown ID must not complete some other turn.
        return None;
    }
    if matches!(kind, Some("response.created" | "error" | "response.failed"))
        && let Some((&id, _)) = current
            .iter()
            .find(|(_, active)| active.rate.initial_sent() && active.response_id.is_none())
    {
        return Some(id);
    }
    current
        .iter()
        .find(|(_, active)| active.rate.initial_sent())
        .map(|(&id, _)| id)
}

// Error-frame auth reload uses the credential that opened the pinned socket.
// It never replays a response.create into a different upstream connection.
async fn apply_error(
    context: &Context,
    event: &Value,
    model: Option<&str>,
) -> Result<crate::relay::outcome::Decision, ()> {
    use crate::relay::outcome::{self, Kind};
    // The shared HTTP classifier decodes at most 64 KiB. Reduce an already
    // parsed WS error to its decision fields so unrelated large metadata cannot
    // hide a signal. The original message is still relayed unchanged.
    let root = event.get("response").unwrap_or(event);
    let error = root.get("error").unwrap_or(root);
    let mut bounded_error = serde_json::Map::new();
    for field in ["type", "code"] {
        if let Some(value) = error
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| value.len() <= 256)
        {
            bounded_error.insert(field.into(), Value::String(value.into()));
        }
    }
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        bounded_error.insert(
            "message".into(),
            Value::String(message.chars().take(8192).collect()),
        );
    }
    for field in ["resets_at", "resets_in_seconds"] {
        if let Some(value) = error.get(field).filter(|value| value.is_number()) {
            bounded_error.insert(field.into(), value.clone());
        }
    }
    let raw = serde_json::to_vec(
        &serde_json::json!({"type":event.get("type"),"status":event.get("status"),"error":bounded_error}),
    )
    .unwrap_or_default();
    let failed = context
        .credentials
        .as_ref()
        .map(|(_, token)| crate::accounts::Tokens {
            access_token: token.to_string(),
            refresh_token: String::new(),
            id_token: None,
        });
    let current_token = if event.get("status").and_then(Value::as_u64) == Some(401)
        || error.get("type").and_then(Value::as_str) == Some("authentication_error")
    {
        if let (Some(relay), Some((account, _)), Some(failed)) =
            (&context.relay, &context.credentials, &failed)
        {
            relay
                .tokens
                .is_current(relay.store.clone(), account, failed)
                .await?
        } else {
            true
        }
    } else {
        true
    };
    let mut decision = outcome::classify(
        Codex,
        axum::http::StatusCode::OK,
        &HeaderMap::new(),
        &raw,
        model,
    );
    if decision.kind == Kind::Revoked
        && context
            .credentials
            .as_ref()
            .is_some_and(|(account, _)| crate::relay::credentials::is_linked(account))
    {
        decision.kind = Kind::Auth;
        decision.signal = None;
    }
    if !current_token && matches!(decision.kind, Kind::Auth | Kind::Revoked) {
        decision.kind = Kind::Auth;
        decision.signal = None;
    }
    if decision.kind == Kind::Auth {
        if let (Some(relay), Some((account, _)), Some(failed)) =
            (&context.relay, &context.credentials, &failed)
        {
            if current_token
                && super::token_retried(relay, &account.id, &failed.access_token, false)
            {
                relay
                    .tokens
                    .auth_failed(
                        relay.store.clone(),
                        context.observability.clone(),
                        account.clone(),
                        failed,
                    )
                    .await?;
            } else {
                match relay
                    .tokens
                    .after_401(
                        relay.store.clone(),
                        context.observability.clone(),
                        account.clone(),
                        failed,
                    )
                    .await?
                {
                    Some((updated, refreshed)) => {
                        super::token_retried(relay, &updated.id, &refreshed.access_token, true);
                    }
                    None => {
                        relay
                            .tokens
                            .cool_unavailable_auth(relay.store.clone(), account, failed)
                            .await?;
                    }
                }
            }
        }
    } else if decision.kind == Kind::Revoked {
        if let (Some(relay), Some((account, _)), Some(failed)) =
            (&context.relay, &context.credentials, &failed)
        {
            relay
                .tokens
                .revoke_if_current(relay.store.clone(), account, failed)
                .await?;
        } else if let (Some(runtime), Some(signal)) = (&context.runtime, decision.signal.take()) {
            runtime.transition(&context.account, signal).await?;
        }
    } else if let (Some(runtime), Some(signal)) = (&context.runtime, decision.signal.take()) {
        runtime.transition(&context.account, signal).await?;
    }
    // No replay of a WS response.create across connections. C7 is resolved
    // on the next placement of this session (including precommit reconnect).
    if decision.kind == Kind::Quota
        && let Some(runtime) = &context.runtime
    {
        runtime.observe_quota_reset(&context.account, decision.quota_reset);
        let _ = runtime
            .pending_quota(
                &context.account,
                crate::account_runtime::quota_session(context.session.key.as_deref()),
                model,
            )
            .await;
    }
    Ok(decision)
}

async fn reconnect_available(context: &Context) -> Result<bool, ()> {
    let Some(relay) = &context.relay else {
        return Ok(false);
    };
    let key = context.session.key.as_deref();
    // Without a stable key, a model-scoped or unchanged account could be
    // reselected immediately. Relay its frame instead of spending a retry.
    if key.is_none() && relay.accounts.eligible(&context.account, None).await {
        return Ok(false);
    }
    let tried = std::collections::BTreeSet::from([context.account.clone()]);
    if relay.select(Codex, key, None, &tried).await?.is_none() {
        return Ok(false);
    }
    relay.exclude_reconnect(key, &context.account);
    Ok(true)
}

fn event_affects_account(event: &Value, account: &str) -> bool {
    event
        .get("data")
        .and_then(|data| data.get("id"))
        .and_then(Value::as_str)
        == Some(account)
}

fn parsed(text: &str) -> Option<Value> {
    (text.len() <= PARSE_LIMIT)
        .then(|| crate::json_unique::from_slice(text.as_bytes()).ok())
        .flatten()
}

fn response_create(text: &str) -> bool {
    super::super::wire_metadata::Metadata::parse(text.as_bytes()).is_some_and(|m| m.create())
}

fn count_type(counts: &mut BTreeMap<String, u64>, event: &Value) {
    let Some(kind) = event
        .get("type")
        .and_then(Value::as_str)
        .and_then(safe_identifier)
    else {
        return;
    };
    if let Some(count) = counts.get_mut(&kind) {
        *count = count.saturating_add(1);
    } else if counts.len() < TYPE_LIMIT {
        counts.insert(kind, 1);
    }
}

fn ws_rate_advice(event: &Value) -> bool {
    event
        .get("headers")
        .or_else(|| event.get("error").and_then(|error| error.get("headers")))
        .and_then(Value::as_object)
        .is_some_and(|headers| {
            headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("retry-after"))
        })
}

fn ws_retry_after(event: &Value) -> Option<Duration> {
    let headers = event
        .get("headers")
        .or_else(|| event.get("error").and_then(|error| error.get("headers")))?
        .as_object()?;
    let value = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))?
        .1;
    let text = value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|secs| secs.to_string()))?;
    let mut sidecopy = HeaderMap::new();
    sidecopy.insert(
        "retry-after",
        axum::http::HeaderValue::from_str(&text).ok()?,
    );
    crate::relay::outcome::header_seconds(&sidecopy, "retry-after")
        .map(|seconds| Duration::from_secs(seconds as u64))
}

fn has_previous_response_id(message: &Message) -> bool {
    let Message::Text(text) = message else {
        return true;
    };
    super::super::wire_metadata::Metadata::parse(text.as_bytes()).is_none_or(|m| m.continuation())
}

struct Delivered {
    bytes: usize,
    request: Option<u64>,
    terminal: Option<&'static str>,
    completed: bool,
}

struct PendingRateResend {
    request: u64,
    message: Message,
    reservation: Reservation,
    rate: Arc<RateBudget>,
    model: Option<String>,
}

struct PendingInitial {
    request: u64,
    message: Message,
    reservation: Reservation,
    rate: Arc<RateBudget>,
}

type Write<S> = BoxFuture<'static, (S, bool, Delivered)>;

fn write<S>(
    mut sink: S,
    message: Message,
    reservation: Reservation,
    delivered: Delivered,
) -> Write<S>
where
    S: Sink<Message> + Send + Unpin + 'static,
    S::Error: Send,
{
    Box::pin(async move {
        let fragment = message.len() > PARSE_LIMIT
            && matches!(&message, Message::Text(_) | Message::Binary(_));
        let success = if fragment {
            let kind = if matches!(&message, Message::Text(_)) {
                Data::Text
            } else {
                Data::Binary
            };
            let bytes = message.into_data();
            let mut success = true;
            for start in (0..bytes.len()).step_by(64 * 1024) {
                let end = (start + 64 * 1024).min(bytes.len());
                let opcode = OpCode::Data(if start == 0 { kind } else { Data::Continue });
                if sink
                    .send(Message::Frame(Frame::message(
                        bytes.slice(start..end),
                        opcode,
                        end == bytes.len(),
                    )))
                    .await
                    .is_err()
                {
                    success = false;
                    break;
                }
                // 각 프레임 사이에 읽기와 자동 pong이 실행될 기회를 준다.
                tokio::task::yield_now().await;
            }
            success
        } else {
            sink.send(message).await.is_ok()
        };
        // flush가 끝나기 전에는 예산을 반환하지 않는다.
        drop(reservation);
        (sink, success, delivered)
    })
}

/// Classification is an observer, never an authorization boundary. Unknown
/// and large client data still belongs to this socket's pinned account.
fn guarded_upstream_write<S>(
    context: Arc<Context>,
    sink: S,
    message: Message,
    reservation: Reservation,
    delivered: Delivered,
    model: Option<String>,
) -> Write<S>
where
    S: Sink<Message> + Send + Unpin + 'static,
    S::Error: Send,
{
    if matches!(message, Message::Text(_) | Message::Binary(_))
        && let Some(relay) = &context.relay
    {
        relay
            .bindings
            .unknown_request_cost(context.session.key.as_deref());
    }
    Box::pin(async move {
        let rate = Arc::new(RateBudget::new(&context));
        if matches!(message, Message::Text(_) | Message::Binary(_)) {
            if !rate.admit(context.runtime.as_ref(), &context.account).await {
                return (
                    sink,
                    false,
                    Delivered {
                        bytes: 0,
                        terminal: Some("rate_wait_timeout"),
                        ..delivered
                    },
                );
            }
            if let Some(runtime) = &context.runtime
                && model.is_none()
                && !runtime.eligible_unknown_model(&context.account).await
            {
                return (
                    sink,
                    false,
                    Delivered {
                        bytes: 0,
                        terminal: Some("routing_policy_blocked"),
                        ..delivered
                    },
                );
            }
        }
        if matches!(message, Message::Text(_) | Message::Binary(_))
            && let (Some(relay), Some(_)) = (&context.relay, &context.runtime)
            && !relay
                .dispatch_data_allowed(
                    context.session.key.as_deref(),
                    &context.account,
                    model.as_deref(),
                )
                .await
        {
            return (
                sink,
                false,
                Delivered {
                    bytes: 0,
                    terminal: Some("routing_policy_blocked"),
                    ..delivered
                },
            );
        }
        if matches!(message, Message::Text(_) | Message::Binary(_)) && !rate.can_send() {
            return (
                sink,
                false,
                Delivered {
                    bytes: 0,
                    terminal: Some("rate_wait_timeout"),
                    ..delivered
                },
            );
        }
        if matches!(message, Message::Text(_) | Message::Binary(_)) {
            rate.initial_dispatch();
            rate.record_forced_send(context.runtime.as_ref(), &context.account);
        }
        write(sink, message, reservation, delivered).await
    })
}

fn flush<S, M>(mut sink: S) -> Write<S>
where
    S: Sink<M> + Send + Unpin + 'static,
    S::Error: Send,
{
    Box::pin(async move {
        let success = sink.flush().await.is_ok();
        (
            sink,
            success,
            Delivered {
                bytes: 0,
                request: None,
                terminal: None,
                completed: false,
            },
        )
    })
}

type RateRedial = BoxFuture<
    'static,
    Result<
        (
            Write<SplitSink<UpstreamSocket, Message>>,
            SplitStream<UpstreamSocket>,
        ),
        &'static str,
    >,
>;

fn redial_rate(context: Arc<Context>, pending: PendingRateResend) -> RateRedial {
    Box::pin(async move {
        if !pending.rate.can_retry() {
            return Err("rate_wait_timeout");
        }
        if !pending
            .rate
            .admit(context.runtime.as_ref(), &context.account)
            .await
        {
            return Err("rate_wait_timeout");
        }
        if let Some(relay) = &context.relay
            && !relay
                .dispatch_data_allowed(
                    context.session.key.as_deref(),
                    &context.account,
                    pending.model.as_deref(),
                )
                .await
        {
            return Err("routing_policy_blocked");
        }
        // Pacing expiry authorizes one final send. The reconnect retains its
        // own bounded dial timeout; an exhausted pacing budget is not a zero
        // duration connect deadline.
        let socket = super::redial_same_account(&context, pending.model.as_deref())
            .await
            .map_err(|_| "upstream_write_error")?;
        let (sink, read) = socket.split();
        let bytes = pending.message.len();
        let reservation = pending.reservation;
        // Count a retry only when its write is actually polled, after a dial.
        let rate = pending.rate;
        let write = Box::pin(async move {
            if let (Some(relay), Some(_)) = (&context.relay, &context.runtime)
                && !relay
                    .dispatch_data_allowed(
                        context.session.key.as_deref(),
                        &context.account,
                        pending.model.as_deref(),
                    )
                    .await
            {
                return (
                    sink,
                    false,
                    Delivered {
                        bytes: 0,
                        request: Some(pending.request),
                        terminal: Some("routing_policy_blocked"),
                        completed: false,
                    },
                );
            }
            if rate.is_committed() {
                return (
                    sink,
                    true,
                    Delivered {
                        bytes: 0,
                        request: Some(pending.request),
                        terminal: None,
                        completed: false,
                    },
                );
            }
            if !rate.can_send() {
                return (
                    sink,
                    false,
                    Delivered {
                        bytes: 0,
                        request: Some(pending.request),
                        terminal: Some("rate_wait_timeout"),
                        completed: false,
                    },
                );
            }
            rate.note_usage_send(&context);
            rate.sent();
            rate.record_forced_send(context.runtime.as_ref(), &context.account);
            write(
                sink,
                pending.message,
                reservation,
                Delivered {
                    bytes,
                    request: Some(pending.request),
                    terminal: None,
                    completed: false,
                },
            )
            .await
        }) as Write<SplitSink<UpstreamSocket, Message>>;
        Ok((write, read))
    })
}

pub(super) async fn run(client: ClientSocket, upstream: UpstreamSocket, context: Context) {
    run_with_read_timeout(client, upstream, context, UPSTREAM_READ_TIMEOUT).await;
}

async fn run_with_read_timeout(
    client: ClientSocket,
    upstream: UpstreamSocket,
    mut context: Context,
    upstream_read_timeout: Duration,
) {
    let (client_sink, mut client_read) = client.split();
    let (upstream_sink, mut upstream_read) = upstream.split();
    let mut client_sink = Some(client_sink);
    let mut upstream_sink = Some(upstream_sink);
    let mut to_client: Option<Write<SplitSink<ClientSocket, Message>>> = None;
    let mut to_upstream: Option<Write<SplitSink<UpstreamSocket, Message>>> = None;
    let mut pending_rate_redial: Option<PendingRateResend> = None;
    let mut rate_redial: Option<RateRedial> = None;
    let mut deferred_client: Option<(Message, Reservation)> = None;
    let mut pending_initial: Option<PendingInitial> = None;
    let mut initial_admission: Option<BoxFuture<'static, bool>> = None;
    let mut initial_ready = false;
    let mut current = BTreeMap::<u64, CurrentCompletion>::new();
    let mut next_request = 0;
    let mut model: Option<String> = None;
    let mut drain_signal = context.drain_signal.take();
    let context = Arc::new(context);
    let mut draining = context
        .relay
        .as_ref()
        .is_some_and(|relay| relay.is_draining());
    let mut health_events = context.runtime.as_ref().map(|runtime| runtime.subscribe());
    let mut client_admitted = false;
    let mut terminal_pending = false;
    let mut counts = BTreeMap::new();
    let mut last_activity = Instant::now();
    let mut upstream_activity = last_activity;
    let mut ending: Option<(Option<CloseFrame>, &'static str)> = None;
    let mut end_deadline = last_activity;
    context.observability.record_ws_connection(
        &context.connection,
        &context.account,
        "ws_open",
        None,
        "open",
    );

    if !draining
        && let Some(runtime) = &context.runtime
        && !runtime.eligible(&context.account, None).await
    {
        ending = Some((
            Some(CloseFrame {
                code: 1000.into(),
                reason: "".into(),
            }),
            "account_ineligible",
        ));
        end_deadline = Instant::now() + CLOSE_TIMEOUT;
    }

    loop {
        draining |= context
            .relay
            .as_ref()
            .is_some_and(|relay| relay.is_draining());
        // Keep an admitted payload here until the write arm is polled, so
        // drain can cancel it without losing the upstream sink or older turns.
        if draining && let Some(pending) = pending_initial.take() {
            initial_admission = None;
            initial_ready = false;
            deferred_client = None;
            current.remove(&pending.request);
        }
        if draining && current.is_empty() && ending.is_none() {
            ending = Some((
                Some(CloseFrame {
                    code: 1001.into(),
                    reason: "".into(),
                }),
                "takeover_drain",
            ));
            end_deadline = Instant::now() + CLOSE_TIMEOUT;
        }
        if ending.is_some() {
            initial_admission = None;
            pending_initial = None;
            initial_ready = false;
            rate_redial = None;
            if pending_rate_redial.take().is_some() {
                to_upstream = None;
            }
            deferred_client = None;
        }
        if ending.is_some() && to_client.is_none() && to_upstream.is_none() {
            break;
        }
        // During a rate wait, retain at most one extra, budgeted client message.
        // Stop client reads once occupied; drain it through the normal path.
        // split 잠금은 poll 사이에 해제되어
        // 큰 쓰기가 진행되는 동안 같은 소켓의 읽기와 자동 pong을 poll할 수 있다.
        tokio::select! {
            _ = async { drain_signal.as_mut().unwrap().await }, if drain_signal.is_some() && !draining && ending.is_none() => {
                draining = true;
            }
            eligible = async { initial_admission.as_mut().unwrap().await }, if initial_admission.is_some() && to_upstream.is_none() && ending.is_none() => {
                initial_admission = None;
                let pending = pending_initial.take().expect("initial admission owns payload");
                draining |= context.relay.as_ref().is_some_and(|relay| relay.is_draining());
                if draining {
                    // Let the loop cancel only this queued turn; any earlier
                    // dispatched turns still drain before the native 1001.
                    pending_initial = Some(pending);
                    continue;
                }
                if !eligible {
                    let forced = pending.rate.state.lock().unwrap_or_else(|e| e.into_inner()).forced;
                    let eligible_now = match &context.runtime {
                        Some(runtime) => runtime.eligible(&context.account, model.as_deref()).await,
                        None => true,
                    };
                    let expired = forced && eligible_now;
                    let reason = if expired { "rate_wait_timeout" } else { "account_ineligible" };
                    if let Some(active) = current.get_mut(&pending.request) {
                        active.completion.record.outcome = reason.into();
                    }
                    ending = Some((Some(CloseFrame { code: (if expired { 1012 } else { 1000 }).into(), reason: "".into() }), reason));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                    continue;
                }
                pending_initial = Some(pending);
                initial_ready = true;
            }
            result = async { rate_redial.as_mut().unwrap().await }, if rate_redial.is_some() && ending.is_none() => {
                rate_redial = None;
                match result {
                    Ok((write, read)) => { upstream_read = read; to_upstream = Some(write); }
                    Err(reason) => {
                        for active in current.values_mut() { active.completion.record.outcome = reason.into(); }
                        ending = Some((Some(CloseFrame { code: (if reason == "rate_wait_timeout" {1012} else {1000}).into(), reason: "".into() }), reason));
                        end_deadline = Instant::now() + CLOSE_TIMEOUT;
                    }
                }
            }
            result = async { to_client.as_mut().unwrap().await }, if to_client.is_some() => {
                let (sink, success, delivered) = result;
                client_sink = Some(sink);
                to_client = None;
                if success {
                    last_activity = Instant::now();
                    if let Some(active) = delivered.request.and_then(|id| current.get_mut(&id)) {
                        active.completion.record.bytes_out = active.completion.record.bytes_out.saturating_add(delivered.bytes as u64);
                    }
                    if let Some(outcome) = delivered.terminal {
                        terminal_pending = false;
                        if let Some(active) = delivered.request.and_then(|id| current.remove(&id)) {
                            if outcome == "ok" && let (Some(relay), Some((account, token))) = (&context.relay, &context.credentials) {
                                super::clear_retried_token(relay, &account.id, token);
                            }
                            active.finish(outcome);
                        }
                        draining |= context.relay.as_ref().is_some_and(|relay| relay.is_draining());
                        if draining && current.is_empty() && ending.is_none() {
                            ending = Some((Some(CloseFrame { code: 1001.into(), reason: "".into() }), "takeover_drain"));
                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        }
                        if current.is_empty() && ending.is_none() && let Some(runtime) = &context.runtime
                            && !runtime.eligible(&context.account, model.as_deref()).await {
                            ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), "account_ineligible"));
                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        }
                        if current.is_empty() && context.close_after_response && delivered.completed && ending.is_none() {
                            ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), "response_complete"));
                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        }
                    }
                } else if ending.is_none() {
                    ending = Some((None, "client_write_error"));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                }
            }
            result = async {
                if initial_ready && to_upstream.is_none() {
                    let pending = pending_initial.take().expect("ready initial payload");
                    initial_ready = false;
                    let sink = upstream_sink.take().expect("initial admission retains upstream sink");
                    let bytes = pending.message.len();
                    let context = context.clone();
                    let dispatch_model = model.clone();
                    let dispatch_receipt = current.get(&pending.request).and_then(|active| active.completion.binding.as_ref()).map(|binding| binding.receipt());
                    to_upstream = Some(Box::pin(async move {
                        if let (Some(relay), Some(_)) = (&context.relay, &context.runtime)
                            && !relay.dispatch_data_allowed(context.session.key.as_deref(), &context.account, dispatch_model.as_deref()).await {
                            return (sink, false, Delivered { bytes: 0, request: Some(pending.request), terminal: Some("routing_policy_blocked"), completed: false });
                        }
                        if !pending.rate.can_send() {
                            return (sink, false, Delivered {bytes:0,request:Some(pending.request),terminal:Some("rate_wait_timeout"),completed:false});
                        }
                        if let Some(receipt) = dispatch_receipt { receipt.dispatched(); }
                        pending.rate.note_usage_send(&context);
                        pending.rate.initial_dispatch();
                        pending.rate.record_forced_send(context.runtime.as_ref(), &context.account);
                        write(sink, pending.message, pending.reservation, Delivered {
                            bytes, request: Some(pending.request), terminal: None, completed: false,
                        }).await
                    }));
                }
                to_upstream.as_mut().unwrap().await
            }, if to_upstream.is_some() || (initial_ready && ending.is_none()) => {
                let (sink, success, delivered) = result;
                upstream_sink = Some(sink);
                to_upstream = None;
                if success { last_activity = Instant::now(); }
                if success {
                    pending_rate_redial = None;
                } else if ending.is_none() && let Some(reason) = delivered.terminal {
                    if let Some(active) = delivered.request.and_then(|id| current.get_mut(&id)) { active.completion.record.outcome = reason.into(); }
                    ending = Some((Some(CloseFrame {code: (if reason == "rate_wait_timeout" {1012} else {1000}).into(), reason:"".into()}), reason));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                } else if ending.is_none()
                    && let Some(pending) = pending_rate_redial.take()
                    && !has_previous_response_id(&pending.message)
                {
                    rate_redial = Some(redial_rate(context.clone(), pending));
                } else if !success && ending.is_none() {
                    for active in current.values_mut() { active.completion.record.outcome = "upstream_error".into(); }
                    ending = Some((None, "upstream_write_error"));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                }
            }
            _ = context.budget.wait_available(), if !client_admitted && to_upstream.is_none() && ending.is_none() => {
                client_admitted = true;
            }
            _ = context.memory_guard.changed(), if ending.is_none() => {
                // Memory protection pauses client reads but leaves existing streams alive.
            }
            incoming = async {
                if let Some((message, reservation)) = deferred_client.take() {
                    (Some(Ok(message)), Some(reservation))
                } else {
                    (client_read.next().await, None)
                }
            }, if ending.is_none() && (
                (deferred_client.is_none() && (pending_initial.is_some() || pending_rate_redial.is_some() || rate_redial.is_some()))
                || ((deferred_client.is_some() || (client_admitted && !context.memory_guard.is_guarded()))
                    && to_upstream.is_none() && pending_initial.is_none() && rate_redial.is_none() && pending_rate_redial.is_none() && !terminal_pending)
            ) => {
                let (message, reservation) = incoming;
                if pending_initial.is_some() || to_upstream.is_some() || pending_rate_redial.is_some() || rate_redial.is_some() {
                    match message {
                        Some(Ok(Message::Ping(_))) => {
                            if to_client.is_none() { to_client = Some(flush(client_sink.take().unwrap())); }
                            continue;
                        }
                        Some(Ok(Message::Pong(_))) => continue,
                        Some(Ok(Message::Close(frame))) => {
                            ending = Some((frame, "client_close"));
                        }
                        None | Some(Err(_)) => ending = Some((None, "client_eof")),
                        Some(Ok(message)) => {
                            let reservation = context.budget.reserve_message(message.len());
                            deferred_client = Some((message, reservation));
                            continue;
                        }
                    }
                    to_upstream = None;
                    initial_admission = None;
                    pending_initial = None;
                    rate_redial = None;
                    pending_rate_redial = None;
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                    continue;
                }
                client_admitted = false;
                last_activity = Instant::now();
                match message {
                    Some(Ok(message)) => {
                        let bytes = message.len();
                        let reservation = reservation.unwrap_or_else(|| context.budget.reserve_message(bytes));
                        let mut request = current.keys().next().copied();
                        if let Message::Text(text) = &message && response_create(text.as_str()) {
                            draining |= context.relay.as_ref().is_some_and(|relay| relay.is_draining());
                            if draining { continue; }
                            model = super::super::wire_metadata::Metadata::parse(text.as_bytes()).and_then(|meta| meta.model());
                            if let Some(runtime) = &context.runtime && !runtime.eligible(&context.account, model.as_deref()).await {
                                ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), "account_ineligible"));
                                end_deadline = Instant::now() + CLOSE_TIMEOUT;
                                continue;
                            }
                            let Some(mut active) = CurrentCompletion::new(&context).await else {
                                ending=Some((Some(CloseFrame {code:1001.into(),reason:"".into()}),"routing_unavailable"));
                                end_deadline=Instant::now()+CLOSE_TIMEOUT;
                                continue;
                            };
                            active.model = model.clone();
                            active.completion.record.model = model.clone();
                            active.completion.dashboard_model(model.as_deref());
                            request = Some(next_request);
                            current.insert(next_request, active);
                            if let Some(active) = current.get_mut(&next_request) {
                                active.last_create = Some((message.clone(), reservation.clone()));
                            }
                            next_request += 1;
                        }
                        // 라이브러리가 예약한 로컬 pong을 반대쪽 전달 완료와 독립적으로 flush한다.
                        if matches!(&message, Message::Ping(_)) && to_client.is_none() {
                            to_client = Some(flush(client_sink.take().unwrap()));
                        }
                        if response_create(if let Message::Text(text) = &message { text.as_str() } else { "" }) && let Some(relay) = &context.relay {
                            let continuation=has_previous_response_id(&message);
                            if continuation {relay.bindings.unknown_request_cost(context.session.key.as_deref());}
                            else {relay.bindings.request_bytes(context.session.key.as_deref(), bytes as u64);}
                        }
                        if let Some(active) = request.and_then(|id| current.get_mut(&id)) { active.completion.input_bytes.fetch_add(bytes as u64, Ordering::Relaxed); }
                        if let Message::Close(frame) = &message {
                            ending = Some((frame.clone(), "client_close"));
                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        }
                        let initial_rate = if let Message::Text(text) = &message {
                            response_create(text.as_str()).then(|| request.and_then(|id| current.get(&id)).map(|active| active.rate.clone())).flatten()
                        } else { None };
                        // Transfer the deferred slot's reservation into the
                        // write. Re-admitting its already charged bytes could
                        // deadlock when the slot itself fills the budget.
                        if let Some(rate) = initial_rate {
                            let request = request.expect("response.create has completion");
                            pending_initial = Some(PendingInitial { request, message, reservation, rate: rate.clone() });
                            let context = context.clone();
                            let model = model.clone();
                            initial_admission = Some(Box::pin(async move {
                                if !rate.admit(context.runtime.as_ref(), &context.account).await { return false; }
                                match &context.runtime {
                                    Some(runtime) => if model.is_some() { runtime.eligible(&context.account, model.as_deref()).await } else { runtime.eligible_unknown_model(&context.account).await },
                                    None => true,
                                }
                            }));
                        } else {
                            let fallback_model = if let Message::Text(text) = &message { super::super::wire_metadata::Metadata::parse(text.as_bytes()).and_then(|meta| meta.model()) } else { None };
                            to_upstream = Some(guarded_upstream_write(context.clone(), upstream_sink.take().unwrap(), message, reservation,
                                Delivered { bytes, request, terminal: None, completed: false }, fallback_model));
                        }
                    }
                    _ => { ending = Some((None, "client_eof")); end_deadline = Instant::now() + CLOSE_TIMEOUT; }
                }
            }
            message = upstream_read.next(), if to_client.is_none() && rate_redial.is_none() && ending.is_none() => {
                last_activity = Instant::now();
                upstream_activity = last_activity;
                match message {
                    Some(Ok(message)) => {
                        let bytes = message.len();
                        if matches!(&message, Message::Ping(_)) && to_upstream.is_none() {
                            to_upstream = Some(flush(upstream_sink.take().unwrap()));
                        }
                        let event = if let Message::Text(text) = &message { parsed(text.as_str()) } else { None };
                        let request = response_completion(&mut current, event.as_ref());
                        let mut suppress_rate_event = false;
                        if let Some(event) = event.as_ref() {
                            if let Some(runtime) = &context.runtime {
                                let active_model = request.and_then(|id| current.get(&id)).and_then(|active| active.model.as_deref());
                                if let Some(relay) = &context.relay {
                                    let asof = request.and_then(|id| current.get(&id))
                                        .and_then(|active| active.rate.state.lock().unwrap_or_else(|e| e.into_inner()).usage_asof)
                                        .unwrap_or(crate::bindings::UsageObservationAsOf {sequence:0,placed_count:0});
                                    runtime.observe_codex_event_asof(&context.account, event, active_model, (&relay.bindings, asof)).await;
                                } else {
                                    runtime.observe_codex_event_for_model(&context.account, event, active_model).await;
                                }
                            } else {
                                context.observability.observe_codex_event(&context.account, event);
                            }
                            if matches!(event.get("type").and_then(Value::as_str), Some("error" | "response.failed")) {
                                if event.get("status").and_then(Value::as_u64) == Some(400)
                                    && safe_error_code(event).as_deref() == Some("websocket_connection_limit_reached")
                                    && let Some(relay) = &context.relay
                                {
                                    relay.ws_capacity.stream_connection_limit(&context.account);
                                }
                                let active_model = request.and_then(|id| current.get(&id)).and_then(|active| active.model.as_deref());
                                // During takeover, relay the current turn's error unchanged:
                                // no auth refresh, health transition, failover, or rate retry.
                                draining |= context.relay.as_ref().is_some_and(|relay| relay.is_draining());
                                let decision = if draining {
                                    None
                                } else {
                                    match apply_error(&context, event, active_model).await {
                                        Ok(decision) => Some(decision),
                                        Err(()) => {
                                            ending = Some((Some(CloseFrame { code: 1011.into(), reason: "".into() }), "health_store_error"));
                                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                                            // Forward the original error even when health persistence fails.
                                            None
                                        }
                                    }
                                };
                                if decision.as_ref().is_some_and(|decision| decision.kind == crate::relay::outcome::Kind::Rate) {
                                    if let Some(runtime) = &context.runtime {
                                        let sent_at = request.and_then(|id| current.get(&id)).map_or_else(
                                            std::time::Instant::now,
                                            |active| active.rate.state.lock().unwrap_or_else(|e| e.into_inner()).sent_at,
                                        );
                                        if let Some(relay)=&context.relay {relay.bindings.note_rate(&context.account,chrono::Utc::now());}
                                        runtime.observe_429_at(&context.account, sent_at, std::time::Instant::now(), ws_retry_after(event), 1.0);
                                    }
                                    if ws_rate_advice(event) && let Some(active) = request.and_then(|id| current.get_mut(&id)) {
                                        active.completion.relayed_rate_429();
                                    }
                                }
                                // Row 13a excludes row 3 rate limits and requires a current request.
                                if current.len() == 1 && request.and_then(|id| current.get(&id)).is_some_and(|active| !active.committed)
                                    && decision.as_ref().is_some_and(|decision| decision.failover() && decision.kind != crate::relay::outcome::Kind::Rate)
                                    && reconnect_available(&context).await.unwrap_or(false) {
                                    if let Some(mut active) = request.and_then(|id| current.remove(&id)) {
                                        active.upstream(Some(event));
                                        active
                                            .completion
                                            .failover(decision.as_ref().expect("decision").reason());
                                        active.finish("failover");
                                    }
                                    ending = Some((Some(CloseFrame { code: 1012.into(), reason: "".into() }), "precommit_failover"));
                                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                                    // C6 row 13a suppresses this pre-commit error.
                                    continue;
                                }
                                if current.len() == 1
                                    && request.and_then(|id| current.get(&id)).is_some_and(|active| !active.committed)
                                    && decision.as_ref().is_some_and(|decision| decision.kind == crate::relay::outcome::Kind::Rate)
                                    && !ws_rate_advice(event)
                                {
                                    let resend = request
                                        .and_then(|id| current.get_mut(&id))
                                        .and_then(|active| {
                                            if !active.rate.can_retry() {
                                                active.completion.relayed_rate_429();
                                                active.completion.record.outcome = "rate_hold_relayed_429".into();
                                                None
                                            } else {
                                                active.last_create.clone()
                                            }
                                        });
                                    if let Some((message, reservation)) = resend
                                        && let Some(sink) = upstream_sink.take()
                                    {
                                        let rate = current[&request.expect("active rate request")].rate.clone();
                                        pending_rate_redial = Some(PendingRateResend {
                                            request: request.expect("active rate request"),
                                            message: message.clone(),
                                            reservation: reservation.clone(),
                                            rate: rate.clone(),
                                            model: current[&request.expect("active rate request")].model.clone(),
                                        });
                                        let runtime = context.runtime.clone();
                                        let relay = context.relay.clone();
                                        let key = context.session.key.clone();
                                        let retry_model = current[&request.expect("active rate request")].model.clone();
                                        let account = context.account.clone();
                                        let bytes = message.len();
                                        let retry_context = context.clone();
                                        to_upstream = Some(Box::pin(async move {
                                            if !rate.admit(runtime.as_ref(), &account).await {
                                                if rate.is_committed() { return (sink,true,Delivered {bytes:0,request,terminal:None,completed:false}); }
                                                return (sink, false, Delivered {bytes:0, request, terminal:Some("rate_wait_timeout"), completed:false});
                                            }
                                            if let (Some(relay), Some(_)) = (&relay, &runtime)
                                                && !relay.dispatch_data_allowed(key.as_deref(), &account, retry_model.as_deref()).await {
                                                if rate.is_committed() { return (sink,true,Delivered {bytes:0,request,terminal:None,completed:false}); }
                                                return (sink, false, Delivered {bytes:0, request, terminal:Some("routing_policy_blocked"), completed:false});
                                            }
                                            if rate.is_committed() { return (sink,true,Delivered {bytes:0,request,terminal:None,completed:false}); }
                                            if !rate.can_send() {
                                                return (sink, false, Delivered {bytes:0,request,terminal:Some("rate_wait_timeout"),completed:false});
                                            }
                                            rate.note_usage_send(&retry_context);
                                            rate.sent();
                                            rate.record_forced_send(runtime.as_ref(), &account);
                                            write(
                                                sink,
                                                message,
                                                reservation,
                                                Delivered { bytes, request, terminal: None, completed: false },
                                            )
                                            .await
                                        }));
                                        suppress_rate_event = true;
                                    } else {
                                        note_client_rate(&context).await;
                                        ending = Some((Some(CloseFrame { code: 1012.into(), reason: "".into() }), "rate_hold"));
                                        end_deadline = Instant::now() + CLOSE_TIMEOUT;
                                        suppress_rate_event = true;
                                    }
                                }
                            }
                            if !suppress_rate_event && crate::relay::outcome::classify(Codex, axum::http::StatusCode::OK, &HeaderMap::new(), &serde_json::to_vec(event).unwrap_or_default(), None).kind == crate::relay::outcome::Kind::Rate {
                                note_client_rate(&context).await;
                            }
                            if !suppress_rate_event {
                                count_type(&mut counts, event);
                            }
                        }
                        if suppress_rate_event {
                            continue;
                        }
                        if let Message::Close(frame) = &message
                            && let Some(pending) = pending_rate_redial.take()
                        {
                            to_upstream = None;
                            if !has_previous_response_id(&pending.message) && pending.rate.can_retry() {
                                rate_redial = Some(redial_rate(context.clone(), pending));
                                continue;
                            }
                            ending = Some((frame.clone(), "upstream_close"));
                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                            continue;
                        }
                        let observed_kind = event.as_ref().and_then(|e|e.get("type")).and_then(Value::as_str);
                        let benign_control = matches!(observed_kind,Some("metadata"|"codex.rate_limits"));
                        let classified = observed_kind.is_some_and(|kind|kind=="error" || kind.starts_with("response."));
                        // Attribution and replay authority are independent. An unknown ID
                        // must not finish another turn, but its publication must stop replay.
                        if !benign_control && (!classified || request.is_none()) && matches!(message, Message::Text(_) | Message::Binary(_)) {
                            for active in current.values_mut() { if active.rate.initial_sent() { active.committed = true; active.last_create = None; active.rate.commit(); } }
                        }
                        let terminal = request.and_then(|id| current.get_mut(&id)).and_then(|active| active.upstream(event.as_ref()));
                        if let Some(active)=request.and_then(|id|current.get_mut(&id)) && (active.committed || terminal.is_some()) { active.last_create = None; active.rate.commit(); }
                        terminal_pending = terminal.is_some();
                        let completed = event.as_ref().is_some_and(|event| event["type"] == "response.completed");
                        if let Message::Close(frame) = &message {
                            ending = Some((frame.clone(), "upstream_close"));
                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        }
                        to_client = Some(write(client_sink.take().unwrap(), message, context.budget.reserve_message(bytes), Delivered { bytes, request, terminal, completed }));
                    }
                    _ => {
                        if let Some(pending) = pending_rate_redial.take()
                            && !has_previous_response_id(&pending.message)
                        {
                            to_upstream = None;
                            if pending.rate.can_retry() {
                                rate_redial = Some(redial_rate(context.clone(), pending));
                                continue;
                            }
                        }
                        for active in current.values_mut() { active.completion.record.outcome = "upstream_error".into(); }
                        ending = Some((None, "upstream_eof")); end_deadline = Instant::now() + CLOSE_TIMEOUT;
                    }
                }
            }
            event = async { health_events.as_mut().unwrap().recv().await }, if health_events.is_some() && ending.is_none() && !draining => {
                if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                    health_events = None;
                }
                let relevant = match event {
                    Ok(event) => event_affects_account(&event, &context.account),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => false,
                };
                if relevant && current.is_empty() && !terminal_pending && let Some(runtime) = &context.runtime
                    && !runtime.eligible(&context.account, model.as_deref()).await {
                    ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), "account_ineligible"));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)), if ending.is_none() && !draining && current.is_empty() && !terminal_pending => {
                if context.relay.as_ref().is_some_and(|relay| relay.bindings.overload_marked(context.session.key.as_deref(), chrono::Utc::now())) {
                    ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), "overload"));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                }
            }
            _ = tokio::time::sleep_until(last_activity + context.idle_close), if ending.is_none() && !draining && to_client.is_none() && to_upstream.is_none() => {
                ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), "idle_close"));
                end_deadline = Instant::now() + CLOSE_TIMEOUT;
            }
            _ = tokio::time::sleep_until(upstream_activity + upstream_read_timeout), if ending.is_none() => {
                for active in current.values_mut() { active.completion.record.outcome = "upstream_error".into(); }
                ending = Some((Some(CloseFrame { code: 1001.into(), reason: "".into() }), "upstream_read_timeout"));
                end_deadline = Instant::now() + CLOSE_TIMEOUT;
            }
            _ = tokio::time::sleep_until(end_deadline), if ending.is_some() => { break; }
        }
    }
    // End waits and release retained slots before the close handshake and the
    // completion snapshot. Direct cancellation still uses the Drop fallback.
    drop(to_upstream);
    drop(initial_admission);
    drop(pending_initial);
    drop(rate_redial);
    drop(pending_rate_redial);
    drop(deferred_client);
    let (frame, reason) = ending.unwrap_or((None, "closed"));
    let code = frame.as_ref().map(|frame| frame.code.into());
    // 종료 handshake도 느린 상대 때문에 무기한 소켓을 붙잡지 않는다.
    let close_client = async {
        if let Some(mut sink) = client_sink {
            if reason == "client_close" {
                let _ = sink.flush().await;
            } else {
                let _ = sink.send(Message::Close(frame.clone())).await;
            }
        }
    };
    let close_upstream = async {
        if let Some(mut sink) = upstream_sink {
            if reason == "upstream_close" {
                let _ = sink.flush().await;
            } else {
                let _ = sink.send(Message::Close(frame.clone())).await;
            }
        }
    };
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
        tokio::join!(close_client, close_upstream);
    })
    .await;
    drop(current);
    context
        .observability
        .capture_ws_types(context.capture_ticket.as_deref(), &counts);
    context.observability.record_ws_connection(
        &context.connection,
        &context.account,
        "ws_close",
        code,
        reason,
    );
}

async fn note_client_rate(context: &Context) {
    if let Some(relay) = &context.relay {
        relay
            .note_client_rate(Codex, &context.account, context.session.key.as_deref())
            .await;
    }
}

#[cfg(test)]
mod tests;
