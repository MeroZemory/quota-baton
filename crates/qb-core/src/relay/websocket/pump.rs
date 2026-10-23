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
    issue_operation::{CallerGuard, IssueOperation},
    observability::{Observability, safe_error_code, safe_identifier},
    relay::launch::DataLaunchError,
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

impl Context {
    pub(super) fn launch_deadline(&self) -> Instant {
        Instant::now()
            + Duration::from_secs(
                self.relay
                    .as_ref()
                    .map_or(15, |relay| relay.request_timeout_secs),
            )
    }

    // A same-account resume may promote an ordinary socket's pinned binding.
    // Observe metadata without replacing the socket or any active completion.
    fn has_managed_binding(&self) -> bool {
        self.relay.as_ref().is_some_and(|relay| {
            self.session.key.as_deref().is_some_and(|key| {
                relay
                    .bindings
                    .launch_info(key, chrono::Utc::now())
                    .is_ok_and(|info| info.managed)
            })
        })
    }

    async fn managed_launch(
        &self,
        deadline: Instant,
    ) -> Result<Option<crate::bindings::LaunchInfo>, DataLaunchError> {
        let Some(relay) = &self.relay else {
            return Ok(None);
        };
        let info = tokio::select! {
            biased;
            _ = relay.wait_for_drain_start() => return Err(DataLaunchError::Unavailable),
            info = relay.resolve_data_launch(None, self.session.key.as_deref(), deadline) => info?,
        };
        let Some(info) = info.filter(|info| info.managed) else {
            return Ok(None);
        };
        if info.account != self.account {
            return Err(DataLaunchError::Changed);
        }
        Ok(Some(info))
    }

    pub(super) async fn guard_launch_model(
        &self,
        model: Option<&str>,
        deadline: Instant,
    ) -> Result<(), DataLaunchError> {
        let Some(info) = self.managed_launch(deadline).await? else {
            return Ok(());
        };
        let relay = self.relay.as_ref().expect("managed launch has relay");
        tokio::select! {
            biased;
            _ = relay.wait_for_drain_start() => Err(DataLaunchError::Unavailable),
            result = relay.admit_data_model(&info, model, deadline) => result.map(|_| ()),
        }
    }

    async fn prepare_launch_create(
        &self,
        requested: Option<&str>,
        deadline: Instant,
    ) -> Result<(Session, String), DataLaunchError> {
        let info = self
            .managed_launch(deadline)
            .await?
            .ok_or(DataLaunchError::Changed)?;
        let relay = self.relay.as_ref().expect("managed launch has relay");
        let model = tokio::select! {
            biased;
            _ = relay.wait_for_drain_start() => return Err(DataLaunchError::Unavailable),
            result = relay.admit_data_model(&info, requested, deadline) => result?,
        }
        .ok_or(DataLaunchError::Invalid)?;
        Ok((
            Session {
                key: Some(info.canonical_key),
                parent: self.session.parent.clone(),
                model: Some(model.clone()),
            },
            model,
        ))
    }
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
        Self::new_for_session(context, &context.session).await
    }

    async fn new_for_session(context: &Context, session: &Session) -> Option<Self> {
        let mut completion = Completion::new(
            context.observability.clone(),
            &Method::GET,
            "/v1/responses",
            Codex,
            &context.headers,
        );
        completion.record.transport = "ws".into();
        completion.session(session);
        completion.runtime = context.runtime.clone();
        if let Some(relay) = &context.relay {
            completion.track(relay.inflight_requests.clone());
            completion.binding = Some(
                relay
                    .bindings
                    .begin_request_async(session.key.as_deref(), &context.account)
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
        if let Some(event) = event {
            self.completion.observe_usage(event);
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

// Connection diagnostics have no turn/session attribution. Reuse the socket's
// random connection ID as req so this bounded record joins ws_open/ws_close.
fn record_connection_error(context: &Context, event: &Value) {
    let mut diagnostic = Completion::new(
        context.observability.clone(),
        &Method::GET,
        "/v1/responses",
        Codex,
        &HeaderMap::new(),
    );
    diagnostic.dashboard_enabled = false;
    diagnostic.record.req = context.connection.clone();
    diagnostic.record.transport = "ws".into();
    diagnostic.record.account = Some(context.account.clone());
    diagnostic.record.status = event
        .get("status")
        .and_then(Value::as_u64)
        .filter(|status| (100..=599).contains(status))
        .map_or(101, |status| status as u16);
    diagnostic.record.upstream_status = Some(diagnostic.record.status);
    diagnostic.record.upstream_error_code = safe_error_code(event);
    diagnostic.finish("ws_connection_error");
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
            self.completion.accounting_cancel();
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
    if decision.kind == Kind::Quota
        && let Some(runtime) = &context.runtime
    {
        runtime.invalidate_quota_rejection(&context.account).await;
    }
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
    if decision.kind == Kind::ModelPermission
        && let Some(relay) = &context.relay
    {
        let deadline = context.launch_deadline();
        if context
            .managed_launch(deadline)
            .await
            .ok()
            .flatten()
            .is_some()
        {
            // Persist the model-specific denial before observing a changed or
            // unchanged menu. Refresh this socket's account without replay.
            tokio::select! {
                biased;
                _ = relay.wait_for_drain_start() => {},
                _ = tokio::time::timeout_at(deadline, relay.launch_catalog(&context.account, true, true)) => {},
            }
        }
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

async fn reconnect_available(context: &Context, model: Option<&str>) -> Result<bool, ()> {
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
    let deadline = context.launch_deadline();
    if let Some(info) = context.managed_launch(deadline).await.map_err(|_| ())? {
        let model = model
            .or(info.current_model.as_deref())
            .ok_or(())?
            .to_owned();
        let managed = super::super::http_data::ManagedHttp { info, model };
        let key = Some(managed.info.canonical_key.as_str());
        let candidates = relay
            .candidates(Codex, key, Some(&managed.model), &tried)
            .await?;
        let (candidates, _) = relay
            .launch_candidates(&managed, candidates, &tried, deadline)
            .await?;
        // A live socket owns its placement. Preview compatibility without
        // publishing a move through that pin; the next upgrade admits it.
        if !matches!(
            relay.bindings.preview(
                Codex,
                key,
                context.session.parent.as_deref(),
                Some(&managed.model),
                &candidates,
                relay.pins.get(Codex, key).as_deref(),
                0
            ),
            crate::bindings::PreviewDecision::Selected { .. }
        ) {
            return Ok(false);
        }
        relay.exclude_reconnect(key, &context.account);
        return Ok(true);
    }
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

// Metadata bounds string fields without allocating input trees. Preserve the
// distinction between an omitted model and an explicitly invalid model.
fn requested_create_model(bytes: &[u8]) -> Result<Option<String>, DataLaunchError> {
    #[derive(serde::Deserialize)]
    struct Requested {
        #[serde(default, deserialize_with = "model")]
        model: Option<String>,
    }
    fn model<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<String>, D::Error> {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        crate::catalog::valid_model_id(&value)
            .then_some(value)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("invalid model"))
    }
    super::super::wire_metadata::Metadata::parse(bytes).ok_or(DataLaunchError::Invalid)?;
    serde_json::from_slice::<Requested>(bytes)
        .map(|request| request.model)
        .map_err(|_| DataLaunchError::Invalid)
}

fn launch_error(error: DataLaunchError) -> Message {
    Message::text(
        serde_json::json!({
            "type": "error",
            "status": error.status().as_u16(),
            "error": {"type": error.code(), "code": error.code(), "message": error.message()},
        })
        .to_string(),
    )
}

fn launch_failure(code: &str) -> Option<DataLaunchError> {
    [
        DataLaunchError::Relaunch,
        DataLaunchError::Invalid,
        DataLaunchError::Busy,
        DataLaunchError::UnsupportedModel,
        DataLaunchError::Unavailable,
        DataLaunchError::PolicyBlocked,
        DataLaunchError::Changed,
    ]
    .into_iter()
    .find(|error| error.code() == code)
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

// The create's caller/deadline owner is separate from metadata operations.
// Only slots and their polled futures retain this guard, never blocking work.
struct CreateLifetime {
    operation: IssueOperation,
    caller: CallerGuard,
    quota_attempted: std::sync::atomic::AtomicBool,
    quota_revalidated: std::sync::atomic::AtomicBool,
}

impl CreateLifetime {
    fn new(deadline: Instant) -> Arc<Self> {
        let operation = IssueOperation::new(deadline);
        let caller = operation.caller_guard();
        Arc::new(Self {
            operation,
            caller,
            quota_attempted: std::sync::atomic::AtomicBool::new(false),
            quota_revalidated: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn live(&self, context: &Context) -> bool {
        !context
            .relay
            .as_ref()
            .is_some_and(|relay| relay.is_draining())
            && self.operation.can_reply()
            && self.operation.check().is_ok()
    }

    fn admit_first_write(&self) -> bool {
        self.operation.can_reply()
            && self.operation.check().is_ok()
            && self.operation.admit().is_ok()
    }
}

async fn native_initial_eligible(
    context: &Context,
    model: Option<&str>,
    unknown_model: bool,
) -> bool {
    match &context.runtime {
        Some(runtime) if unknown_model => runtime.eligible_unknown_model(&context.account).await,
        Some(runtime) => runtime.eligible(&context.account, model).await,
        None => true,
    }
}

// Only this unsent-create adapter calls the shared wait. Rate resends and WS
// error frames keep their existing row13/C6.3 paths and cannot enter here.
async fn eligible_unsent_create(
    context: &Context,
    model: Option<&str>,
    unknown_model: bool,
    create: &CreateLifetime,
) -> bool {
    if !create.live(context) {
        return false;
    }
    if native_initial_eligible(context, model, unknown_model).await {
        return create.live(context);
    }
    if !create.live(context) {
        return false;
    }
    let (Some(relay), Some(key)) = (&context.relay, context.session.key.as_deref()) else {
        return false;
    };
    // One engine invocation for the entire create, across inspection/pacing.
    // Renewals INSIDE that invocation preserve the admitted envelope.
    if create.quota_attempted.swap(true, Ordering::AcqRel) {
        return false;
    }
    let hint = crate::relay::quota_wait::wait_ws_quota(
        relay,
        key,
        model,
        &context.account,
        &create.operation,
        &create.caller,
    )
    .await;
    if !create.live(context) {
        return false;
    }
    let Some(hint) = hint else {
        return false;
    };
    if hint.source != context.account
        || !matches!(
            hint.end,
            crate::relay::quota_wait::WaitEnd::Reset
                | crate::relay::quota_wait::WaitEnd::NativeCacheReset
                | crate::relay::quota_wait::WaitEnd::Revoked
        )
    {
        return false;
    }
    // Reset or revoked authority is only a hint to recheck THIS pinned account.
    // Never invoke the chooser, replace the socket, or mint dispatch authority.
    let eligible =
        native_initial_eligible(context, model, unknown_model).await && create.live(context);
    if eligible {
        create.quota_revalidated.store(true, Ordering::Release);
    }
    #[cfg(feature = "testkit")]
    if eligible && hint.end != crate::relay::quota_wait::WaitEnd::NativeCacheReset {
        relay.observe_ws_quota_wait("reselect", &context.account, create.operation.deadline());
    }
    eligible
}

struct PendingInitial {
    request: u64,
    message: Message,
    reservation: Reservation,
    rate: Arc<RateBudget>,
    create: Arc<CreateLifetime>,
}

struct PendingInspection {
    message: Message,
    reservation: Reservation,
    session: Session,
    model: Option<String>,
    managed: bool,
    create: Arc<CreateLifetime>,
}

#[derive(Debug)]
enum InspectFailure {
    Expired,
    Ineligible,
    ReservationUnavailable,
}
type InitialInspection = BoxFuture<'static, Result<CurrentCompletion, InspectFailure>>;

#[derive(Debug, PartialEq, Eq)]
enum InitialAdmission {
    Ready,
    Expired,
    Rejected { expired: bool },
}

fn inspect_initial(context: Arc<Context>, pending: &PendingInspection) -> InitialInspection {
    let session = Session {
        key: pending.session.key.clone(),
        parent: pending.session.parent.clone(),
        model: pending.session.model.clone(),
    };
    let model = pending.model.clone();
    let managed = pending.managed;
    let create = pending.create.clone();
    #[cfg(feature = "testkit")]
    let message = pending.message.clone();
    #[cfg(feature = "testkit")]
    let deadline = create.operation.deadline();
    Box::pin(async move {
        if !create.live(&context) {
            return Err(InspectFailure::Expired);
        }
        #[cfg(feature = "testkit")]
        if !wait_dispatch_until(
            &context,
            &message,
            OrdinaryDispatchStage::BeforeEligibility,
            deadline,
        )
        .await
        {
            return Err(InspectFailure::Ineligible);
        }
        if !eligible_unsent_create(&context, model.as_deref(), false, &create).await {
            return Err(if create.live(&context) {
                InspectFailure::Ineligible
            } else {
                InspectFailure::Expired
            });
        }
        #[cfg(feature = "testkit")]
        if !wait_dispatch_until(
            &context,
            &message,
            OrdinaryDispatchStage::BeforeLease,
            deadline,
        )
        .await
        {
            return Err(InspectFailure::ReservationUnavailable);
        }
        if !create.live(&context) {
            return Err(InspectFailure::Expired);
        }
        let active = if managed {
            CurrentCompletion::new_for_session(&context, &session).await
        } else {
            CurrentCompletion::new(&context).await
        };
        if !create.live(&context) {
            return Err(InspectFailure::Expired);
        }
        active.ok_or(InspectFailure::ReservationUnavailable)
    })
}

fn admit_initial(
    context: Arc<Context>,
    pending: &PendingInitial,
    model: Option<String>,
    managed: bool,
) -> BoxFuture<'static, InitialAdmission> {
    let rate = pending.rate.clone();
    let create = pending.create.clone();
    #[cfg(feature = "testkit")]
    let message = pending.message.clone();
    Box::pin(async move {
        if !create.live(&context) {
            return InitialAdmission::Expired;
        }
        let paced = rate.admit(context.runtime.as_ref(), &context.account).await;
        if !create.live(&context) {
            return InitialAdmission::Expired;
        }
        let unknown_model = !managed && model.is_none();
        let eligible = paced
            && !rate.initial_sent()
            && !rate.is_committed()
            && eligible_unsent_create(&context, model.as_deref(), unknown_model, &create).await;
        #[cfg(feature = "testkit")]
        let eligible = eligible
            && (managed
                || wait_ordinary_dispatch(&context, &message, OrdinaryDispatchStage::AfterPacing)
                    .await);
        // Recheck after the retained-frame gate too: a real poll may change
        // eligibility during that suspension. The one-attempt flag prevents
        // a new quota interval if this create already used the engine.
        let eligible = eligible
            && !rate.initial_sent()
            && !rate.is_committed()
            && eligible_unsent_create(&context, model.as_deref(), unknown_model, &create).await;
        if !create.live(&context) {
            return InitialAdmission::Expired;
        }
        if eligible {
            return InitialAdmission::Ready;
        }
        // Keep the native false-result recheck in this independently polled future.
        let forced = rate.state.lock().unwrap_or_else(|e| e.into_inner()).forced;
        let eligible_now = match &context.runtime {
            Some(runtime) => runtime.eligible(&context.account, model.as_deref()).await,
            None => true,
        };
        if !create.live(&context) {
            return InitialAdmission::Expired;
        }
        InitialAdmission::Rejected {
            expired: forced && eligible_now,
        }
    })
}

type LaunchPreparation = BoxFuture<'static, Result<(Session, String), DataLaunchError>>;
type CreateAdmission = BoxFuture<'static, Result<String, DataLaunchError>>;

// An ordinary observer has not admitted this frame as managed data. Reject it
// if promotion wins the final gate; keep the socket and older completions alive.
const PROMOTED_BEFORE_WRITE: &str = "launch_promoted_before_write";

#[cfg(feature = "testkit")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum OrdinaryDispatchStage {
    AfterPacing,
    BeforeWrite,
    BeforeEligibility,
    BeforeLease,
}

#[cfg(feature = "testkit")]
struct OrdinaryDispatchGate {
    relay: std::sync::Weak<crate::relay::RelayState>,
    session: String,
    payload: Vec<u8>,
    stage: OrdinaryDispatchStage,
    arrived: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(feature = "testkit")]
static ORDINARY_DISPATCH_GATES: Mutex<Vec<OrdinaryDispatchGate>> = Mutex::new(Vec::new());

#[cfg(feature = "testkit")]
impl crate::relay::RelayState {
    /// Test-only pause immediately before initial eligibility or lease inspection.
    pub fn pause_ws_initial_inspection(
        self: &Arc<Self>,
        session: &str,
        payload: &[u8],
        before_lease: bool,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        pause_ordinary_dispatch(
            self,
            session,
            payload,
            if before_lease {
                OrdinaryDispatchStage::BeforeLease
            } else {
                OrdinaryDispatchStage::BeforeEligibility
            },
        )
    }

    /// Pause the specified ordinary frame after pacing, before observing promotion.
    pub fn pause_ws_ordinary_after_pacing(
        self: &Arc<Self>,
        session: &str,
        payload: &[u8],
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        pause_ordinary_dispatch(self, session, payload, OrdinaryDispatchStage::AfterPacing)
    }

    /// Pause the specified ordinary frame after awaited policy checks, before write.
    pub fn pause_ws_ordinary_before_write(
        self: &Arc<Self>,
        session: &str,
        payload: &[u8],
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        pause_ordinary_dispatch(self, session, payload, OrdinaryDispatchStage::BeforeWrite)
    }
}

#[cfg(feature = "testkit")]
fn pause_ordinary_dispatch(
    relay: &Arc<crate::relay::RelayState>,
    session: &str,
    payload: &[u8],
    stage: OrdinaryDispatchStage,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (arrived, observed) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let owner = Arc::downgrade(relay);
    let mut gates = ORDINARY_DISPATCH_GATES
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    gates.retain(|gate| gate.relay.strong_count() > 0 && !gate.arrived.is_closed());
    assert!(
        !gates.iter().any(|gate| gate.relay.ptr_eq(&owner)
            && gate.session == session
            && gate.payload == payload
            && gate.stage == stage),
        "ordinary frame gate already pending"
    );
    gates.push(OrdinaryDispatchGate {
        relay: owner,
        session: session.into(),
        payload: payload.into(),
        stage,
        arrived,
        release: gate,
    });
    (observed, release)
}

#[cfg(feature = "testkit")]
async fn wait_ordinary_dispatch(
    context: &Context,
    message: &Message,
    stage: OrdinaryDispatchStage,
) -> bool {
    wait_dispatch_until(context, message, stage, context.launch_deadline()).await
}

#[cfg(feature = "testkit")]
async fn wait_dispatch_until(
    context: &Context,
    message: &Message,
    stage: OrdinaryDispatchStage,
    deadline: Instant,
) -> bool {
    let Some(relay) = &context.relay else {
        return true;
    };
    let payload: &[u8] = match message {
        Message::Text(text) => text.as_bytes(),
        Message::Binary(bytes) => bytes,
        _ => return true,
    };
    let gate = {
        let owner = Arc::downgrade(relay);
        let mut gates = ORDINARY_DISPATCH_GATES
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        gates
            .iter()
            .position(|gate| {
                gate.relay.ptr_eq(&owner)
                    && Some(gate.session.as_str()) == context.session.key.as_deref()
                    && gate.payload == payload
                    && gate.stage == stage
            })
            .map(|at| gates.remove(at))
    };
    if let Some(gate) = gate {
        let _ = gate.arrived.send(());
        tokio::select! {
            biased;
            _ = relay.wait_for_drain_start() => return false,
            result = tokio::time::timeout_at(deadline, gate.release) => {
                return result.is_ok_and(|result| result.is_ok());
            }
        }
    }
    true
}

#[cfg(feature = "testkit")]
struct CreateDispatchGate {
    relay: std::sync::Weak<crate::relay::RelayState>,
    arrived: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
#[cfg(feature = "testkit")]
static CREATE_DISPATCH_GATES: Mutex<Vec<CreateDispatchGate>> = Mutex::new(Vec::new());

#[cfg(feature = "testkit")]
pub(super) fn pause_create_dispatch(
    relay: &Arc<crate::relay::RelayState>,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (arrived, observed) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let owner = Arc::downgrade(relay);
    let mut gates = CREATE_DISPATCH_GATES
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    gates.retain(|gate| gate.relay.strong_count() > 0);
    assert!(
        !gates.iter().any(|gate| gate.relay.ptr_eq(&owner)),
        "create gate already pending"
    );
    gates.push(CreateDispatchGate {
        relay: owner,
        arrived,
        release: gate,
    });
    (observed, release)
}

// Only a successful unsent quota revalidation may bypass failed-refresh backoff.
// This refresh supplies evidence to the unchanged native admission below; it
// neither accepts a model nor changes the socket's source or create deadline.
fn admit_create_after_quota(
    context: Arc<Context>,
    model: Option<String>,
    create: Arc<CreateLifetime>,
) -> CreateAdmission {
    Box::pin(async move {
        if !create.live(&context) {
            return Err(DataLaunchError::Unavailable);
        }
        if create.quota_revalidated.load(Ordering::Acquire)
            && let Some(relay) = &context.relay
        {
            let view = relay
                .catalogs
                .view(&context.account)
                .map_err(|_| DataLaunchError::Unavailable)?;
            if matches!(
                view.observation,
                crate::catalog::Observation::RefreshFailed { previous: None, .. }
            ) {
                tokio::select! {
                    biased;
                    _ = relay.wait_for_drain_start() => return Err(DataLaunchError::Unavailable),
                    result = tokio::time::timeout_at(
                        create.operation.deadline(),
                        relay.launch_catalog(&context.account, true, true),
                    ) => {
                        result.map_err(|_| DataLaunchError::Unavailable)?
                            .map_err(|_| DataLaunchError::Unavailable)?;
                    }
                }
            }
        }
        if !create.live(&context) {
            return Err(DataLaunchError::Unavailable);
        }
        admit_create(context, model, create.operation.deadline()).await
    })
}

fn admit_create(
    context: Arc<Context>,
    model: Option<String>,
    deadline: Instant,
) -> CreateAdmission {
    Box::pin(async move {
        #[cfg(feature = "testkit")]
        if let Some(relay) = &context.relay {
            let gate = {
                let owner = Arc::downgrade(relay);
                let mut gates = CREATE_DISPATCH_GATES
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                gates
                    .iter()
                    .position(|gate| gate.relay.ptr_eq(&owner))
                    .map(|at| gates.remove(at))
            };
            if let Some(gate) = gate {
                let _ = gate.arrived.send(());
                tokio::select! {
                    biased;
                    _ = relay.wait_for_drain_start() => return Err(DataLaunchError::Unavailable),
                    _ = tokio::time::timeout_at(deadline, gate.release) => {},
                }
            }
        }
        let (_, model) = context
            .prepare_launch_create(model.as_deref(), deadline)
            .await?;
        if let Some(relay) = &context.relay
            && !relay
                .dispatch_data_allowed(
                    context.session.key.as_deref(),
                    &context.account,
                    Some(&model),
                )
                .await
        {
            return Err(DataLaunchError::PolicyBlocked);
        }
        Ok(model)
    })
}

type Write<S> = BoxFuture<'static, (S, bool, Delivered)>;

fn write_initial<S>(
    sink: S,
    message: Message,
    reservation: Reservation,
    delivered: Delivered,
    create: Arc<CreateLifetime>,
) -> Write<S>
where
    S: Sink<Message> + Send + Unpin + 'static,
    S::Error: Send,
{
    Box::pin(async move {
        #[cfg(test)]
        let (sink, success, delivered) = write(
            tests::TimerWriteSink::new(sink, &create),
            message,
            reservation,
            delivered,
        )
        .await;
        #[cfg(test)]
        let result = (sink.into_inner(), success, delivered);
        #[cfg(not(test))]
        let result = write(sink, message, reservation, delivered).await;
        // Keep the paired caller alive through the first poll and physical
        // write backpressure. Admission is not a delivery/flush receipt.
        drop(create);
        result
    })
}

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

/// Legacy classification is an observer. Managed data has already passed its
/// metadata check and revalidates the account/model after pacing, before send.
fn guarded_upstream_write<S>(
    context: Arc<Context>,
    sink: S,
    message: Message,
    reservation: Reservation,
    delivered: Delivered,
    model: Option<String>,
    managed: bool,
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
            if managed
                && let Err(error) = context
                    .guard_launch_model(model.as_deref(), context.launch_deadline())
                    .await
            {
                return (
                    sink,
                    false,
                    Delivered {
                        bytes: 0,
                        terminal: Some(error.code()),
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
        #[cfg(feature = "testkit")]
        if !managed
            && !wait_ordinary_dispatch(&context, &message, OrdinaryDispatchStage::BeforeWrite).await
        {
            return (
                sink,
                false,
                Delivered {
                    bytes: 0,
                    request: None,
                    terminal: Some("takeover_drain"),
                    ..delivered
                },
            );
        }
        // The observer fallback also awaits pacing/model/policy. It must not
        // write ordinary data after a concurrent resume promotes this binding.
        // Keep its observer completion (which may belong to an older turn).
        if matches!(message, Message::Text(_) | Message::Binary(_))
            && !managed
            && context.has_managed_binding()
        {
            return (
                sink,
                false,
                Delivered {
                    bytes: 0,
                    request: None,
                    terminal: Some(PROMOTED_BEFORE_WRITE),
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
        context
            .guard_launch_model(pending.model.as_deref(), context.launch_deadline())
            .await
            .map_err(|error| error.code())?;
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
        let socket = super::redial_same_account(&context, pending.model.as_deref()).await?;
        let (sink, read) = socket.split();
        let bytes = pending.message.len();
        let reservation = pending.reservation;
        // Count a retry only when its write is actually polled, after a dial.
        let rate = pending.rate;
        let write = Box::pin(async move {
            if let Err(error) = context
                .guard_launch_model(pending.model.as_deref(), context.launch_deadline())
                .await
            {
                return (
                    sink,
                    false,
                    Delivered {
                        bytes: 0,
                        request: Some(pending.request),
                        terminal: Some(error.code()),
                        completed: false,
                    },
                );
            }
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
    let mut pending_launch_create: Option<(Message, Reservation, Arc<CreateLifetime>)> = None;
    let mut launch_preparation: Option<LaunchPreparation> = None;
    let mut pending_launch_error: Option<Message> = None;
    let mut pending_initial: Option<PendingInitial> = None;
    let mut initial_admission: Option<BoxFuture<'static, InitialAdmission>> = None;
    let mut pending_inspection: Option<PendingInspection> = None;
    let mut initial_inspection: Option<InitialInspection> = None;
    let mut create_admission: Option<CreateAdmission> = None;
    let mut initial_write_admission: Option<BoxFuture<'static, Result<(), &'static str>>> = None;
    let mut initial_ready = false;
    let mut current = BTreeMap::<u64, CurrentCompletion>::new();
    let mut next_request = 0;
    let mut connection_errors = 0_u8;
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
    let mut managed_socket = if draining {
        false
    } else {
        match context.managed_launch(context.launch_deadline()).await {
            Ok(info) => info.is_some(),
            Err(error) => {
                pending_launch_error = Some(launch_error(error));
                ending = Some((
                    Some(CloseFrame {
                        code: 1000.into(),
                        reason: "".into(),
                    }),
                    error.code(),
                ));
                end_deadline = Instant::now() + CLOSE_TIMEOUT;
                false
            }
        }
    };
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
        if draining && pending_launch_create.take().is_some() {
            launch_preparation = None;
            deferred_client = None;
        }
        if draining && pending_inspection.take().is_some() {
            initial_inspection = None;
            deferred_client = None;
        }
        if draining && let Some(pending) = pending_initial.take() {
            initial_admission = None;
            create_admission = None;
            initial_write_admission = None;
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
            launch_preparation = None;
            pending_launch_create = None;
            pending_inspection = None;
            initial_inspection = None;
            initial_admission = None;
            create_admission = None;
            initial_write_admission = None;
            pending_initial = None;
            initial_ready = false;
            rate_redial = None;
            if pending_rate_redial.take().is_some() {
                to_upstream = None;
            }
            deferred_client = None;
        }
        if to_client.is_none()
            && let Some(message) = pending_launch_error.take()
        {
            let bytes = message.len();
            to_client = Some(write(
                client_sink.take().unwrap(),
                message,
                context.budget.reserve_message(bytes),
                Delivered {
                    bytes,
                    request: None,
                    terminal: None,
                    completed: false,
                },
            ));
        }
        if ending.is_some() && to_client.is_none() && to_upstream.is_none() {
            break;
        }
        // During a rate wait, retain at most one extra, budgeted client message.
        // Keep reading controls with that slot occupied; another application
        // frame closes the connection instead of growing a queue.
        // split 잠금은 poll 사이에 해제되어
        // 큰 쓰기가 진행되는 동안 같은 소켓의 읽기와 자동 pong을 poll할 수 있다.
        // The write arm can consume this slot and admit its first write while
        // the same select is still polling. Keep identity without extending the
        // caller's lifetime; the deadline applies only to that pending create.
        let pending_create = pending_launch_create
            .as_ref()
            .map(|(_, _, create)| create)
            .or_else(|| pending_inspection.as_ref().map(|pending| &pending.create))
            .or_else(|| pending_initial.as_ref().map(|pending| &pending.create))
            .map(|create| (Arc::downgrade(create), create.operation.deadline()));
        let pending_create_deadline = pending_create.as_ref().map(|(_, deadline)| *deadline);
        #[cfg(test)]
        tests::observe_outer_create_snapshot(
            pending_create.as_ref(),
            initial_ready && pending_initial.is_some() && client_admitted,
        );
        let pending_inspection_active = pending_inspection.is_some();
        let pending_launch_active = pending_launch_create.is_some();
        let pending_client_admission = pending_launch_create.is_some()
            || pending_inspection_active
            || pending_initial.is_some()
            || to_upstream.is_some()
            || pending_rate_redial.is_some()
            || rate_redial.is_some();
        tokio::select! {
            _ = async {
                match pending_create_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            }, if pending_create_deadline.is_some() && ending.is_none() => {
                // A Pending sink flush must not turn an admission deadline into
                // a physical completion timeout after the write arm admitted it.
                let still_pending = pending_launch_create.as_ref().map(|(_, _, create)| create)
                    .or_else(|| pending_inspection.as_ref().map(|pending| &pending.create))
                    .or_else(|| pending_initial.as_ref().map(|pending| &pending.create))
                    .is_some_and(|current| pending_create.as_ref()
                        .is_some_and(|(timed, _)| timed.ptr_eq(&Arc::downgrade(current))));
                #[cfg(test)]
                tests::observe_outer_create_timer(still_pending);
                if still_pending {
                    ending = Some((Some(CloseFrame { code:1000.into(), reason:"".into() }), "request_timeout"));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                }
            }
            _ = async { drain_signal.as_mut().unwrap().await }, if drain_signal.is_some() && !draining && ending.is_none() => {
                draining = true;
            }
            result = async { launch_preparation.as_mut().unwrap().await }, if launch_preparation.is_some() && ending.is_none() => {
                launch_preparation = None;
                let (message, reservation, create) = pending_launch_create.take().expect("launch preparation owns payload");
                draining |= context.relay.as_ref().is_some_and(|relay| relay.is_draining());
                if draining { continue; }
                let (session, accepted_model) = match result {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        pending_launch_error = Some(launch_error(error));
                        if !matches!(error, DataLaunchError::UnsupportedModel | DataLaunchError::Invalid) {
                            ending = Some((Some(CloseFrame { code: 1000.into(), reason: "".into() }), error.code()));
                            end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        }
                        continue;
                    }
                };
                let pending = PendingInspection { message, reservation, session, model: Some(accepted_model), managed: true, create };
                initial_inspection = Some(inspect_initial(context.clone(), &pending));
                pending_inspection = Some(pending);
            }
            result = async { initial_inspection.as_mut().unwrap().await }, if initial_inspection.is_some() && ending.is_none() => {
                initial_inspection = None;
                let pending = pending_inspection.take().expect("inspection owns payload");
                draining |= context.relay.as_ref().is_some_and(|relay| relay.is_draining());
                if draining { continue; }
                let mut active = match result {
                    Ok(active) => active,
                    Err(InspectFailure::Expired) => {
                        ending = Some((Some(CloseFrame { code:1000.into(), reason:"".into() }), "request_timeout"));
                        end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        continue;
                    }
                    Err(InspectFailure::Ineligible) => {
                        ending = Some((Some(CloseFrame { code:1000.into(), reason:"".into() }), "account_ineligible"));
                        end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        continue;
                    }
                    Err(InspectFailure::ReservationUnavailable) => {
                        if pending.managed { pending_launch_error = Some(launch_error(DataLaunchError::Changed)); }
                        ending = Some((Some(CloseFrame { code:1001.into(), reason:"".into() }), "routing_unavailable"));
                        end_deadline = Instant::now() + CLOSE_TIMEOUT;
                        continue;
                    }
                };
                model = pending.model.clone();
                active.model = model.clone();
                active.completion.record.model = model.clone();
                active.completion.dashboard_model(model.as_deref());
                active.last_create = Some((pending.message.clone(), pending.reservation.clone()));
                active.completion.input_bytes.fetch_add(pending.message.len() as u64, Ordering::Relaxed);
                if let Some(relay) = &context.relay {
                    if has_previous_response_id(&pending.message) { relay.bindings.unknown_request_cost(pending.session.key.as_deref()); }
                    else { relay.bindings.request_bytes(pending.session.key.as_deref(), pending.message.len() as u64); }
                }
                let rate = active.rate.clone();
                let request = next_request;
                next_request += 1;
                current.insert(request, active);
                let initial = PendingInitial { request, message: pending.message, reservation: pending.reservation, rate, create: pending.create };
                initial_admission = Some(admit_initial(context.clone(), &initial, model.clone(), pending.managed));
                pending_initial = Some(initial);
            }
            admission = async { initial_admission.as_mut().unwrap().await }, if initial_admission.is_some() && to_upstream.is_none() && ending.is_none() => {
                initial_admission = None;
                let pending = pending_initial.take().expect("initial admission owns payload");
                draining |= context.relay.as_ref().is_some_and(|relay| relay.is_draining());
                if draining {
                    // Let the loop cancel only this queued turn; any earlier
                    // dispatched turns still drain before the native 1001.
                    pending_initial = Some(pending);
                    continue;
                }
                if admission == InitialAdmission::Expired {
                    if let Some(active) = current.get_mut(&pending.request) { active.completion.record.outcome = "request_timeout".into(); }
                    ending = Some((Some(CloseFrame { code:1000.into(), reason:"".into() }), "request_timeout"));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                    continue;
                }
                if let InitialAdmission::Rejected { expired } = admission {
                    let reason = if expired { "rate_wait_timeout" } else { "account_ineligible" };
                    if let Some(active) = current.get_mut(&pending.request) {
                        active.completion.record.outcome = reason.into();
                    }
                    ending = Some((Some(CloseFrame { code: (if expired { 1012 } else { 1000 }).into(), reason: "".into() }), reason));
                    end_deadline = Instant::now() + CLOSE_TIMEOUT;
                    continue;
                }
                // Promotion may also happen while this unsent turn waits for rate admission.
                managed_socket |= context.has_managed_binding();
                if managed_socket {
                    // Logging metadata intentionally loses some protocol model IDs.
                    // Reparse the preserved frame, including explicit null/invalid.
                    let requested = match requested_create_model(&pending.message.clone().into_data()) {
                        Ok(requested) => requested,
                        Err(error) => {
                            pending_launch_error = Some(launch_error(error));
                            if let Some(mut active) = current.remove(&pending.request) {
                                active.completion.record.outcome = error.code().into();
                            }
                            continue;
                        }
                    };
                    create_admission = Some(admit_create_after_quota(context.clone(), requested, pending.create.clone()));
                } else {
                    initial_ready = true;
                }
                pending_initial = Some(pending);
            }
            result = async { create_admission.as_mut().unwrap().await }, if create_admission.is_some() && to_upstream.is_none() && ending.is_none() => {
                create_admission = None;
                draining |= context.relay.as_ref().is_some_and(|relay| relay.is_draining());
                // pending_initial still owns the unsent payload and the sink is
                // still in upstream_sink. Drain cancels just this turn above.
                if draining { continue; }
                match result {
                    Ok(accepted_model) => {
                        if let Some(active) = pending_initial.as_ref().and_then(|pending| current.get_mut(&pending.request)) {
                            active.model = Some(accepted_model.clone());
                            active.completion.record.model = Some(accepted_model.clone());
                            active.completion.dashboard_model(Some(&accepted_model));
                        }
                        initial_ready = true;
                    }
                    Err(error) => {
                        pending_launch_error = Some(launch_error(error));
                        if let Some(pending) = pending_initial.take()
                            && let Some(mut active) = current.remove(&pending.request) {
                                active.completion.record.outcome = error.code().into();
                        }
                        if matches!(error, DataLaunchError::UnsupportedModel | DataLaunchError::Invalid) {
                            continue;
                        }
                        ending = Some((Some(CloseFrame { code:1000.into(), reason:"".into() }), error.code()));
                        end_deadline = Instant::now() + CLOSE_TIMEOUT;
                    }
                }
            }
            result = async { rate_redial.as_mut().unwrap().await }, if rate_redial.is_some() && ending.is_none() => {
                rate_redial = None;
                match result {
                    Ok((write, read)) => { upstream_read = read; to_upstream = Some(write); }
                    Err(reason) => {
                        if managed_socket && let Some(error) = launch_failure(reason) { pending_launch_error = Some(launch_error(error)); }
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
                        if current.is_empty() && !pending_launch_active && !pending_inspection_active && ending.is_none() && let Some(runtime) = &context.runtime
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
                    // Retain the payload and sink in cancellable slots while the
                    // final policy check awaits. A queued create must not claim
                    // a move before another socket actually dispatches its turn.
                    let admission = if managed_socket {
                        Ok(())
                    } else {
                        let admission = initial_write_admission.get_or_insert_with(|| {
                            let context = context.clone();
                            let dispatch_model = pending_initial.as_ref()
                                .and_then(|pending| current.get(&pending.request))
                                .and_then(|active| active.model.clone());
                            #[cfg(feature = "testkit")]
                            let message = pending_initial.as_ref().expect("ready initial payload").message.clone();
                            Box::pin(async move {
                                if let (Some(relay), Some(_)) = (&context.relay, &context.runtime)
                                    && !relay.dispatch_data_allowed(context.session.key.as_deref(), &context.account, dispatch_model.as_deref()).await {
                                    return Err("routing_policy_blocked");
                                }
                                #[cfg(feature = "testkit")]
                                if !wait_ordinary_dispatch(&context, &message, OrdinaryDispatchStage::BeforeWrite).await {
                                    return Err("takeover_drain");
                                }
                                #[cfg(test)]
                                tests::wait_outer_timer_write_gate().await;
                                Ok(())
                            })
                        });
                        admission.await
                    };
                    initial_write_admission = None;
                    let pending = pending_initial.take().expect("ready initial payload");
                    initial_ready = false;
                    let sink = upstream_sink.take().expect("initial admission retains upstream sink");
                    let bytes = pending.message.len();
                    let managed = managed_socket;
                    if let Err(reason) = admission {
                        return (sink, false, Delivered { bytes: 0, request: Some(pending.request), terminal: Some(reason), completed: false });
                    }
                    // No await from these final vetoes through accounting and
                    // the first write poll. Ordinary model metadata is not admission.
                    if context.relay.as_ref().is_some_and(|relay| relay.is_draining()) {
                        return (sink, false, Delivered { bytes: 0, request: Some(pending.request), terminal: Some("takeover_drain"), completed: false });
                    }
                    if !managed && context.has_managed_binding() {
                        return (sink, false, Delivered { bytes: 0, request: Some(pending.request), terminal: Some(PROMOTED_BEFORE_WRITE), completed: false });
                    }
                    if managed && let Some(relay) = &context.relay {
                        // A live provisional pin can redirect to its adopted C8 key.
                        let dispatch_info = context.session.key.as_deref()
                            .and_then(|key| relay.bindings.launch_info(key, chrono::Utc::now()).ok());
                        if !dispatch_info.as_ref().is_some_and(|info| info.managed
                            && info.account == context.account
                            && relay.bindings.permits_dispatch(Some(&info.canonical_key), &context.account)) {
                            return (sink, false, Delivered { bytes: 0, request: Some(pending.request), terminal: Some("routing_policy_blocked"), completed: false });
                        }
                    }
                    if !pending.rate.can_send() {
                        return (sink, false, Delivered {bytes:0,request:Some(pending.request),terminal:Some("rate_wait_timeout"),completed:false});
                    }
                    if !pending.create.admit_first_write() {
                        return (sink, false, Delivered { bytes:0, request:Some(pending.request), terminal:Some("request_timeout"), completed:false });
                    }
                    if let Some(active) = current.get_mut(&pending.request) {
                        active.completion.accounting_attempt(&context.account);
                        if let Some(binding) = &active.completion.binding {
                            binding.dispatched();
                        }
                    }
                    pending.rate.note_usage_send(&context);
                    pending.rate.initial_dispatch();
                    pending.rate.record_forced_send(context.runtime.as_ref(), &context.account);
                    to_upstream = Some(write_initial(sink, pending.message, pending.reservation, Delivered {
                        bytes, request: Some(pending.request), terminal: None, completed: false,
                    }, pending.create));
                }
                to_upstream.as_mut().unwrap().await
            }, if to_upstream.is_some() || (initial_ready && ending.is_none()) => {
                let (sink, success, delivered) = result;
                upstream_sink = Some(sink);
                to_upstream = None;
                if !success && delivered.terminal == Some("takeover_drain") {
                    draining = true;
                    if let Some(request) = delivered.request { current.remove(&request); }
                    continue;
                }
                if !success && delivered.terminal == Some(PROMOTED_BEFORE_WRITE) {
                    managed_socket = true;
                    pending_launch_error = Some(launch_error(DataLaunchError::Changed));
                    if let Some(mut active) = delivered.request.and_then(|id| current.remove(&id)) {
                        active.completion.record.outcome = DataLaunchError::Changed.code().into();
                    }
                    continue;
                }
                if success { last_activity = Instant::now(); }
                if success {
                    pending_rate_redial = None;
                } else if ending.is_none() && let Some(reason) = delivered.terminal {
                    if managed_socket && let Some(error) = launch_failure(reason) { pending_launch_error = Some(launch_error(error)); }
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
                // A retained application frame must not conceal later control
                // frames. Drain that slot only after pending admission/write ends.
                if pending_client_admission {
                    (client_read.next().await, None)
                } else if let Some((message, reservation)) = deferred_client.take() {
                    (Some(Ok(message)), Some(reservation))
                } else {
                    (client_read.next().await, None)
                }
            }, if ending.is_none() && pending_launch_error.is_none() && (
                pending_client_admission
                || ((deferred_client.is_some() || (client_admitted && !context.memory_guard.is_guarded()))
                    && to_upstream.is_none() && pending_launch_create.is_none() && !pending_inspection_active && pending_initial.is_none() && rate_redial.is_none() && pending_rate_redial.is_none() && !terminal_pending)
            ) => {
                let (message, reservation) = incoming;
                if pending_client_admission {
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
                            if deferred_client.is_some() {
                                // Exactly one extra application frame is retained.
                                // Do not overwrite its reservation or grow a queue.
                                ending = Some((Some(CloseFrame { code: 1008.into(), reason: "".into() }), "pending_client_overflow"));
                            } else {
                                let reservation = context.budget.reserve_message(message.len());
                                deferred_client = Some((message, reservation));
                                continue;
                            }
                        }
                    }
                    to_upstream = None;
                    launch_preparation = None;
                    pending_launch_create = None;
                    pending_inspection = None;
                    initial_inspection = None;
                    initial_admission = None;
                    create_admission = None;
                    initial_write_admission = None;
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
                        let request = current.keys().next().copied();
                        // Upgrade-time classification cannot outlive authoritative promotion.
                        // Keep the original message/reservation and use normal managed preparation.
                        if !managed_socket && matches!(&message, Message::Text(_) | Message::Binary(_)) {
                            managed_socket = context.has_managed_binding();
                        }
                        if managed_socket && matches!(&message, Message::Text(_) | Message::Binary(_)) {
                            let data = message.clone().into_data();
                            let Some(meta) = super::super::wire_metadata::Metadata::parse(&data) else {
                                pending_launch_error = Some(launch_error(DataLaunchError::Invalid));
                                continue;
                            };
                            if meta.create() {
                                draining |= context.relay.as_ref().is_some_and(|relay| relay.is_draining());
                                if draining { continue; }
                                let requested = match requested_create_model(&data) {
                                    Ok(model) => model,
                                    Err(error) => { pending_launch_error = Some(launch_error(error)); continue; }
                                };
                                let context = context.clone();
                                let create = CreateLifetime::new(context.launch_deadline());
                                let preparation_create = create.clone();
                                launch_preparation = Some(Box::pin(async move {
                                    if !preparation_create.live(&context) { return Err(DataLaunchError::Unavailable); }
                                    let prepared = context.prepare_launch_create(requested.as_deref(), preparation_create.operation.deadline()).await;
                                    if !preparation_create.live(&context) { return Err(DataLaunchError::Unavailable); }
                                    prepared
                                }));
                                pending_launch_create = Some((message, reservation, create));
                                continue;
                            }
                        }
                        if let Message::Text(text) = &message && response_create(text.as_str()) {
                            draining |= context.relay.as_ref().is_some_and(|relay| relay.is_draining());
                            if draining { continue; }
                            let pending = PendingInspection {
                                model: super::super::wire_metadata::Metadata::parse(text.as_bytes()).and_then(|meta| meta.model()),
                                session: Session { key: context.session.key.clone(), parent: context.session.parent.clone(), model: context.session.model.clone() },
                                managed: false, create: CreateLifetime::new(context.launch_deadline()), message, reservation,
                            };
                            initial_inspection = Some(inspect_initial(context.clone(), &pending));
                            pending_inspection = Some(pending);
                            continue;
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
                        let fallback_model = if let Message::Text(text) = &message { super::super::wire_metadata::Metadata::parse(text.as_bytes()).and_then(|meta| meta.model()) } else { None };
                        to_upstream = Some(guarded_upstream_write(context.clone(), upstream_sink.take().unwrap(), message, reservation,
                            Delivered { bytes, request, terminal: None, completed: false }, fallback_model, managed_socket));
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
                        if request.is_none() && connection_errors < 16
                            && let Some(error) = event.as_ref().filter(|event| {
                                matches!(event["type"].as_str(), Some("error" | "response.failed"))
                                    || (matches!(event["type"].as_str(), Some("response.done" | "response.completed"))
                                        && event["response"]["status"] == "failed")
                            })
                        {
                            record_connection_error(&context, error);
                            connection_errors += 1;
                        }
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
                                let active_model = request.and_then(|id| current.get(&id)).and_then(|active| active.model.clone());
                                // During takeover, relay the current turn's error unchanged:
                                // no auth refresh, health transition, failover, or rate retry.
                                draining |= context.relay.as_ref().is_some_and(|relay| relay.is_draining());
                                let decision = if draining {
                                    None
                                } else {
                                    match apply_error(&context, event, active_model.as_deref()).await {
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
                                    if let Some(active) = request.and_then(|id| current.get_mut(&id)) {
                                        // This dispatched attempt failed. A replay is
                                        // not a normal baseline turn and cannot replace
                                        // its move claim with later successful usage.
                                        active.completion.accounting_cancel();
                                        active.completion.record.cache_usage = None;
                                    }
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
                                    && !(managed_socket && decision.as_ref().is_some_and(|decision| decision.kind == crate::relay::outcome::Kind::ModelPermission))
                                    && reconnect_available(&context, active_model.as_deref()).await.unwrap_or(false) {
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
                                            if let Err(error) = retry_context.guard_launch_model(retry_model.as_deref(), retry_context.launch_deadline()).await {
                                                if rate.is_committed() { return (sink,true,Delivered {bytes:0,request,terminal:None,completed:false}); }
                                                return (sink, false, Delivered {bytes:0, request, terminal:Some(error.code()), completed:false});
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
                if relevant && current.is_empty() && !pending_launch_active && !pending_inspection_active && !terminal_pending && let Some(runtime) = &context.runtime
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
    drop(launch_preparation);
    drop(pending_launch_create);
    drop(initial_inspection);
    drop(pending_inspection);
    drop(initial_admission);
    drop(create_admission);
    drop(initial_write_admission);
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
